<script setup lang="ts">
// The "Add Data Source" / "Edit" dialog: pick a type, fill one form,
// review the TOML that will be written. One form writes one source —
// the `[[groups]]` entry, its `ingest` step and its `render_markdown`
// step — and editing a source reopens the same form over all three.
// Ingest fields sit on the main screen; a provider's render fields sit
// under a "Rendering" heading and land on the render step.
//
// Two fields carry the identity, and only one of them is permanent.
// **Name** is what you type and what every screen shows; it is free
// text and always editable. **Id** is the group's id: the directory on
// disk and the prefix inside every `qmd_path` the index holds, so
// changing it is a migration rather than an edit — it is derived from
// the name once, at creation, and read-only forever after. Both land on
// the `[[groups]]` entry; the steps written under it carry neither.

// A descriptor with a `credentialService` also gets a **Connection**
// block: which latchkey account to use, "Latchkey auth", which runs
// latchkey's browser login, and "Test connection", which calls the
// provider's own probe (`datalib-step probe <type>`). What comes back is not just a
// green tick — it names the account actually reached, and it fills
// every `probe:` field's checklist, the render step's included. A
// label picker built from the live account is the difference between a
// filter that works and a filter that is a spelling test.
import { computed, onUnmounted, ref, watch } from "vue";
import {
  CATALOG,
  KIND_LABELS,
  entryKey,
  filterCatalog,
  type CatalogEntry,
  type Field,
} from "@/config/catalog";
import {
  buildSource,
  fieldIsActive,
  fieldsFor,
  paramsObject,
  seedFieldValues,
  slugify,
  stepIdFor,
  suggestId,
  type ConfiguredGroup,
  type FieldValues,
  type SourceSteps,
} from "@/config/sourceSteps";
import {
  latchkeyService,
  probeSource,
  startLatchkeyConnect,
  latchkeyConnectStatus,
  type ProbeItem,
  type ProbeReport,
  type StoredAccount,
} from "@/api";
import { iconUrl } from "@/config/icons";
import { ingestReach } from "@/config/ingestMethods";
import { isDesktopApp, pickPath } from "@/desktop";
import ProbeItemPicker from "@/components/ProbeItemPicker.vue";

const props = defineProps<{
  /// Group ids already in the config, plus the id of every step outside
  /// a group, so a new source can't land on a tree that exists.
  takenIds: Set<string>;
  /// Present → edit that source instead of creating one. `steps` holds
  /// whichever of its two steps the config has; one it lacks is written
  /// on save, and the form says so.
  editing?: {
    group: ConfiguredGroup;
    entry: CatalogEntry;
    steps: SourceSteps;
  } | null;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (
    e: "submit",
    payload: {
      /// The group's id.
      id: string;
      /// The group's name as typed. The caller writes it on the group —
      /// as part of `groupBody` when creating, by renaming when editing.
      name: string;
      entry: CatalogEntry;
      /// The `[[groups]]` block, when this dialog is creating a source.
      /// Null when editing: the group already exists.
      groupBody: string | null;
      /// The `[[steps]]` blocks: the ingest step, then the render step
      /// for a provider that renders.
      stepsBody: string;
      /// The render step's composed id, for the caller to wire into the
      /// fan-ins. Null for a provider that renders nothing.
      renderId: string | null;
    },
  ): void;
}>();

type Stage = "pick" | "configure";

const mode = computed<"create" | "edit">(() => (props.editing ? "edit" : "create"));
const isEdit = computed(() => mode.value === "edit");

const stage = ref<Stage>(props.editing ? "configure" : "pick");
const query = ref("");
const chosen = ref<CatalogEntry | null>(props.editing?.entry ?? null);

/// Blank means "no name" — a group with none is shown by its id, so the
/// field takes the id as its placeholder rather than pre-filling one,
/// and clearing it removes the key.
const name = ref(props.editing?.group.name ?? "");
/// The group's id: the directory its steps write under. Typed while
/// creating, fixed while editing.
const id = ref(props.editing?.group.id ?? "");
const values = ref<FieldValues>({});
/// Once the id has been typed into directly, the name stops driving it.
/// A derived id is a convenience, never something that overwrites a
/// choice the user made.
const idTouched = ref(false);

/// Can this provider render at all? A download-only provider (a photo
/// catalog, a media tree) has no text to render, and no choice to
/// offer.
const providerRenders = computed(() => !!chosen.value && chosen.value.renderStep !== false);

/// Whether this source wants its render step. On by default: mirrored
/// data that is never rendered reaches neither the grid nor search, so
/// off is the deliberate answer. Editing seeds it from what the config
/// already has.
const renderWanted = ref(props.editing ? !!props.editing.steps.render : true);

/// Does this source write a render step — the provider can, and this
/// source asked for it.
const renders = computed(() => providerRenders.value && renderWanted.value);

/// The fields the form shows for one phase: the descriptor's, less any
/// whose gate is shut.
function activeFields(phase: "download" | "render"): Field[] {
  return chosen.value
    ? fieldsFor(chosen.value, phase).filter((f) => fieldIsActive(f, values.value))
    : [];
}

const downloadFields = computed(() => activeFields("download"));
const renderFields = computed(() => (renders.value ? activeFields("render") : []));

/// The ingest fields the main form renders. The latchkey account is one
/// of this descriptor's fields like any other — it lands on the same
/// params target and is written by the same code — but it is *shown*
/// inside the Connection block, next to the button that populates it.
/// Rendering it twice is the bug this exists to prevent.
const formFields = computed(() =>
  downloadFields.value.filter((f) => f !== (accountField.value as Field | undefined)),
);

/// The form in two parts: the ingest fields, then the render fields
/// under their own heading, drawn by one template.
const sections = computed(() => [
  { key: "ingest", heading: null as string | null, fields: formFields.value },
  { key: "render", heading: "Rendering", fields: renderFields.value },
]);

/// Kinds whose control is small enough to sit beside its label rather
/// than under it. A tickbox and a spinner are each narrower than the
/// words naming them, so a row apiece is mostly empty space.
const INLINE_KINDS = new Set<Field["kind"]>(["bool", "int"]);

/// Steps this source is missing, which saving writes. Only while
/// editing — a hand-edited group can have one step and not the other —
/// and worth a sentence, since Save then does more than change a value.
const missingSteps = computed<string[]>(() => {
  if (!props.editing) return [];
  const out: string[] = [];
  if (!props.editing.steps.ingest) out.push(stepIdFor(id.value, "download"));
  if (renders.value && !props.editing.steps.render) out.push(stepIdFor(id.value, "render"));
  return out;
});

/// A render step this source has that its provider does not write — a
/// hand-written one under a download-only type. Saving removes it, and
/// that is worth a sentence for the same reason a missing step is.
const orphanRender = computed<string | null>(() =>
  props.editing && !renders.value && props.editing.steps.render
    ? props.editing.steps.render.id
    : null,
);

const groups = computed(() => {
  const matches = filterCatalog(query.value);
  return (["api", "export", "local"] as const)
    .map((kind) => ({ kind, label: KIND_LABELS[kind], entries: matches.filter((e) => e.kind === kind) }))
    .filter((g) => g.entries.length > 0);
});

// Flat list in display order, for keyboard navigation.
const flat = computed(() => groups.value.flatMap((g) => g.entries));
const cursor = ref(0);
watch(query, () => (cursor.value = 0));

/// A dropdown's options, plus the current value when it isn't one of
/// them — so a hand-edited config renders as what it says instead of as
/// a blank select, and saving round-trips it.
function selectOptions(f: Field & { kind: "select" }): { value: string; label: string }[] {
  const current = values.value[f.target];
  if (typeof current !== "string" || f.options.some((o) => o.value === current)) {
    return f.options;
  }
  return [...f.options, { value: current, label: `${current} (not a known value)` }];
}

if (props.editing) values.value = seedFieldValues(props.editing.entry, props.editing.steps);

function choose(entry: CatalogEntry) {
  if (!entry.wizard) return;
  chosen.value = entry;
  // No name typed yet, so the id starts from the catalog's default.
  // Typing a name re-derives it, until the id is touched directly.
  id.value = suggestId(props.takenIds, "", entry.defaultName);
  idTouched.value = false;
  values.value = seedFieldValues(entry);
  stage.value = "configure";
}

/// Name → id, one way, while creating and while the id is untouched.
/// A name that slugifies to nothing (punctuation only, or a non-Latin
/// script) leaves the catalog default in place rather than producing
/// something unrecognizable.
watch(name, (next) => {
  if (mode.value !== "create" || idTouched.value || !chosen.value) return;
  id.value = suggestId(props.takenIds, slugify(next), chosen.value.defaultName);
});

function onPickKeydown(e: KeyboardEvent) {
  if (e.key === "ArrowDown") {
    cursor.value = Math.min(cursor.value + 1, flat.value.length - 1);
    e.preventDefault();
  } else if (e.key === "ArrowUp") {
    cursor.value = Math.max(cursor.value - 1, 0);
    e.preventDefault();
  } else if (e.key === "Enter") {
    const entry = flat.value[cursor.value];
    if (entry) choose(entry);
    e.preventDefault();
  } else if (e.key === "Escape") {
    emit("close");
  }
}

/// The id is a single path segment: the steps written under it are
/// `<id>/ingest` and `<id>/render_markdown`, and the loader composes
/// those itself.
const RESERVED = new Set(["system", "unified_index"]);
const groupId = computed(() => id.value.trim());
const idError = computed(() => {
  const n = groupId.value;
  if (!n) return "An id is required.";
  if (RESERVED.has(n)) return `"${n}" is reserved — it names a directory the pipeline owns.`;
  if (n === "." || n === "..") return "The id must not be '.' or '..'.";
  if (n.startsWith("-")) return "The id must not start with '-'.";
  if (!/^[A-Za-z0-9._-]+$/.test(n))
    return "Use only letters, digits, '.', '_' and '-' — the id becomes a directory.";
  if (mode.value === "create" && props.takenIds.has(n))
    return `"${n}" is already configured.`;
  return null;
});

/// Fields the provider's Rust struct declares non-optional — a
/// `PathBuf` rather than an `Option<PathBuf>` — so a config missing one
/// fails at deserialize time rather than at sync time. Caught here so
/// the message lands under the field instead of in a job log.
const missingRequired = computed(() =>
  [...downloadFields.value, ...renderFields.value]
    .filter((f) => "required" in f && f.required)
    .filter((f) => String(values.value[f.target] ?? "").trim() === "")
    .map((f) => f.label),
);

const canSubmit = computed(() => !idError.value && missingRequired.value.length === 0);

/// What this dialog writes: the group while creating, and the source's
/// steps either way.
const source = computed(() =>
  chosen.value
    ? buildSource({
        entry: chosen.value,
        group: groupId.value,
        name: name.value,
        values: values.value,
        withGroup: mode.value === "create",
        renders: renders.value,
      })
    : null,
);

const preview = computed(() =>
  source.value
    ? `${source.value.groupBody ? `${source.value.groupBody}\n\n` : ""}${source.value.stepsBody}`
    : "",
);

function listText(field: Field): string {
  const v = values.value[field.target];
  return Array.isArray(v) ? (v as string[]).join(", ") : "";
}
function setListText(field: Field, text: string) {
  values.value[field.target] = text
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

/// A path field gets a native picker in the desktop app and a bare
/// text box in a browser, which is the best a browser can do: it never
/// hands back a filesystem path, and the path here is one on the
/// machine running the backend. See docs/dev/wizard_file_pickers.md.
const canPick = isDesktopApp();

/// Keyed by field target: a dialog that was denied rather than
/// canceled, which has no signal of its own and would otherwise look
/// like a dead button.
const pickFailed = ref<Record<string, string>>({});

async function browse(f: Field) {
  if (f.kind !== "path") return;
  const result = await pickPath({
    picks: f.picks ?? "dir",
    title: f.pickTitle ?? f.label,
    // Re-editing a source reopens near its current value.
    startAt: String(values.value[f.target] ?? ""),
    extensions: f.extensions,
  });
  if (result.outcome === "picked") {
    values.value[f.target] = result.path;
    delete pickFailed.value[f.target];
  } else if (result.outcome === "unavailable") {
    pickFailed.value[f.target] = result.reason;
  }
  // Canceled: leave the field exactly as it was, and say nothing.
}

// Connection: which latchkey account, and what it can reach

/// The params a probe authenticates with: the ingest step's, as this
/// form would write them. That is where the credentials and the mode
/// live, and the render step's pickers are filled from the same answer.
const probeParams = computed<Record<string, unknown> | null>(() =>
  chosen.value ? paramsObject(chosen.value, values.value, "download") : null,
);

/// The latchkey service this source authenticates against — and only
/// while the params the form would write reach an origin. An import
/// (`ingestReach` says `local`) has nothing to log in to, however the
/// descriptor is labelled, so it gets no Connection section.
const service = computed(() => {
  const entry = chosen.value;
  if (!entry?.credentialService) return null;
  return ingestReach(entry.type, probeParams.value ?? {}) === "origin"
    ? entry.credentialService
    : null;
});

/// The one field, if any, that holds a latchkey account. There is at
/// most one per descriptor: a step mirrors one identity.
const accountField = computed(
  () =>
    downloadFields.value.find((f) => f.kind === "text" && f.latchkey) as
      | (Field & { kind: "text" })
      | undefined,
);

const accounts = ref<StoredAccount[] | null>(null);
const authOptions = ref<string[]>([]);
/// Whether latchkey holds this service already. Starts true so nothing
/// offers to register one before the answer is in.
const serviceRegistered = ref(true);
/// How to invoke latchkey on the machine running the backend. `npx`
/// until the server says otherwise, so a command is never shown naming
/// a binary that isn't there.
const latchkeyCli = ref("npx -y latchkey");
/// Why the account list is empty, when latchkey could not be asked.
/// Shown as a note, not an error — the field is still typable.
const accountsError = ref<string | null>(null);

async function loadAccounts() {
  const name = service.value;
  if (!name) return;
  accounts.value = null;
  accountsError.value = null;
  try {
    const info = await latchkeyService(name);
    accounts.value = info.accounts;
    authOptions.value = info.auth_options;
    serviceRegistered.value = info.registered;
    latchkeyCli.value = info.cli;
    accountsError.value = info.error;
  } catch (e) {
    accounts.value = [];
    accountsError.value = String(e);
  }
}

/// The registration this dialog would send, and only while it would
/// actually be used: a service latchkey already holds keeps whatever
/// login its owner gave it, and re-registering is refused anyway.
const wouldRegister = computed(() =>
  !serviceRegistered.value ? chosen.value?.credentialRegister : undefined,
);

/// Offer the button when latchkey says this service can do a browser
/// login, or when the service is ours to register with one. Offering it
/// for a service that can do neither would produce a failure that reads
/// like a bug in datalib.
const canConnect = computed(
  () => authOptions.value.includes("browser") || !!chosen.value?.credentialRegister,
);

/// A service latchkey holds that cannot do a browser login. Its owner
/// set it up by hand, so the dialog says how to add a credential the
/// same way rather than offering to change it.
const setOnlyService = computed(
  () => serviceRegistered.value && !authOptions.value.includes("browser"),
);

/// Set by the button on a service latchkey holds without a browser
/// login: converting one means taking it apart and putting it back,
/// which destroys the credentials stored under it. The commands are
/// shown; running them is the owner's call, not this dialog's.
const showConversion = ref(false);

/// What converting `service` to a browser login actually takes, in the
/// order it has to happen and naming this machine's latchkey.
const conversionCommands = computed(() => {
  const reg = chosen.value?.credentialRegister;
  const name = service.value;
  if (!reg || !name) return "";
  const lk = latchkeyCli.value;
  const params = JSON.stringify(reg.login_flow_params);
  return [
    `${lk} auth clear ${name} --all`,
    `${lk} services deregister ${name}`,
    `${lk} services register ${name} \\`,
    `  --base-api-url="${reg.base_api_url}" \\`,
    `  --login-url="${reg.login_url}" \\`,
    `  --login-flow=${reg.login_flow} \\`,
    `  --login-flow-params='${params}'`,
  ].join("\n");
});

/// Pasting a credential works on every service latchkey holds, browser
/// login or not: a cookie-capture service reports `["browser", "set"]`
/// and takes `auth set` per account just the same. Said out loud
/// wherever a Connect button would otherwise read as the only way in.
const canPasteCredential = computed(
  () => authOptions.value.includes("set") || !!wouldRegister.value,
);

const connect = ref<{ state: "idle" | "running" | "ok" | "failed"; message: string }>({
  state: "idle",
  message: "",
});

/// Set on unmount so an in-flight poll stops rather than writing into a
/// dialog that is gone.
let closed = false;
onUnmounted(() => {
  closed = true;
});

async function connectViaLatchkey() {
  const name = service.value;
  if (!name || connect.value.state === "running") return;
  if (setOnlyService.value) {
    showConversion.value = true;
    return;
  }
  connect.value = { state: "running", message: "A browser window should open. Finish the login there." };
  try {
    const started = await startLatchkeyConnect(name, accountValue.value, wouldRegister.value);
    for (;;) {
      await new Promise((r) => setTimeout(r, 1500));
      if (closed) return;
      const status = await latchkeyConnectStatus(started.id);
      if (status.status === "running") continue;
      if (status.status === "ok") {
        connect.value = { state: "ok", message: "Connected. The account list below is refreshed." };
        // The point of connecting was to add an account; showing the
        // stale list would hide the one just added.
        await loadAccounts();
      } else {
        connect.value = { state: "failed", message: status.output || "The login did not complete." };
      }
      return;
    }
  } catch (e) {
    connect.value = { state: "failed", message: String(e) };
  }
}

/// The account currently in the form. Empty means "latchkey's unnamed
/// default", which is addressed by writing no `account` at all — so
/// empty is a real answer, not a missing one.
const accountValue = computed(() =>
  accountField.value ? String(values.value[accountField.value.target] ?? "").trim() : "",
);

// The probe

const probe = ref<{
  state: "idle" | "running" | "ok" | "failed";
  message: string;
  report: ProbeReport | null;
}>({ state: "idle", message: "", report: null });


/// Can "Test connection" be offered here at all?
const canProbe = computed(() => !!chosen.value?.canProbe && !!probeParams.value);

async function testConnection() {
  const entry = chosen.value;
  const params = probeParams.value;
  if (!entry || !params || probe.value.state === "running") return;
  probe.value = { state: "running", message: "", report: null };
  try {
    const report = await probeSource(entry.type, params);
    probe.value = {
      state: "ok",
      message: "",
      report,
    };
  } catch (e) {
    probe.value = { state: "failed", message: probeFailure(e), report: null };
  }
}

/// A probe failure as something to read. What arrives is the step's own
/// stderr — one `error: ` line per link in its cause chain, and for a
/// credential problem a numbered setup recipe after them — wrapped in a
/// JS `Error`. Strip the two layers of prefix that add nothing; the
/// lines themselves are the message, and the template keeps them.
function probeFailure(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e);
  return raw
    .split("\n")
    .map((line) => line.replace(/^\s*error:\s*/, ""))
    .join("\n")
    .trim();
}

/// The failure in one line, which is the part that says what went
/// wrong. Everything after it is how to fix it.
const probeHeadline = computed(() => probe.value.message.split("\n")[0] ?? "");
/// The rest, kept as written: it is a numbered recipe with commands in
/// it, and reflowing it into a paragraph is what made it unreadable.
const probeDetail = computed(() => probe.value.message.split("\n").slice(1).join("\n").trim());

/// What a `probe:` field should offer, given what came back.
function probeOptions(field: Field): ProbeItem[] {
  const report = probe.value.report;
  if (!report || field.kind !== "string_list" || !field.probe) return [];
  switch (field.probe) {
    // A render filter matches only what emails are filed in, never a
    // Gmail flag.
    case "mailboxes":
      return report.items.filter((i) => i.kind === "mailbox");
    case "conversations":
      return report.items.filter((i) => i.kind === "conversation");
    default:
      return report.items.filter((i) => i.kind !== "conversation");
  }
}

/// What the field holds today, as the array the picker binds to.
function chosenValues(field: Field): string[] {
  const v = values.value[field.target];
  return Array.isArray(v) ? (v as string[]) : [];
}

/// Chosen values the probed account does not have.
function unknownValues(field: Field): string[] {
  const options = probeOptions(field);
  if (options.length === 0) return [];
  const known = new Set(options.map((i) => i.path));
  return chosenValues(field).filter((p) => !known.has(p));
}

/// What a field's picker is a picker *of*, for the sentences around it.
/// An email account has folders and labels, a Claude account has
/// conversations, and calling any of them "labels" reads as a bug.
const PROBE_NOUNS = {
  labels: "labels",
  mailboxes: "folders",
  conversations: "conversations",
} as const;

/// The same word for what the probe actually came back with.
const probedNoun = computed(() => {
  const items = probe.value.report?.items ?? [];
  return items.some((i) => i.kind === "conversation")
    ? PROBE_NOUNS.conversations
    : PROBE_NOUNS.labels;
});

// Load the account list as soon as there is a service to load it for:
// on open in edit mode, and on picking a tile in create mode.
watch(
  service,
  (name) => {
    if (name) void loadAccounts();
  },
  { immediate: true },
);

function submit() {
  if (!canSubmit.value || !chosen.value || !source.value) return;
  emit("submit", {
    id: groupId.value,
    name: name.value.trim(),
    entry: chosen.value,
    groupBody: source.value.groupBody,
    stepsBody: source.value.stepsBody,
    renderId: source.value.renderId,
  });
}
</script>

<template>
  <!-- The backdrop deliberately does not close this dialog: a stray
       click beside a half-filled form would discard every field in it
       with nothing to undo it. The × and Cancel are the ways out. -->
  <div class="wiz-backdrop">
    <div class="wiz" role="dialog" aria-modal="true" :aria-label="isEdit ? 'Edit source' : 'Add data source'">
      <header class="wiz-head">
        <h2>{{ isEdit ? `Edit ${name || id}` : "Add a data source" }}</h2>
        <button class="wiz-x" aria-label="Close" @click="emit('close')">×</button>
      </header>

      <!-- Stage 1: pick a type -->
      <div v-if="stage === 'pick'" class="wiz-body">
        <input
          v-model="query"
          class="wiz-filter"
          type="search"
          placeholder="Search sources — slack, mail, photos…"
          autofocus
          @keydown="onPickKeydown"
        />
        <p v-if="flat.length === 0" class="wiz-empty">No source type matches “{{ query }}”.</p>
        <div v-for="g in groups" :key="g.kind" class="wiz-group">
          <h3>{{ g.label }}</h3>
          <div class="wiz-tiles">
            <button
              v-for="e in g.entries"
              :key="entryKey(e)"
              class="wiz-tile"
              :class="{ soon: !e.wizard, cursor: flat[cursor] === e }"
              :disabled="!e.wizard"
              :title="e.wizard ? e.blurb : 'No guided setup yet — add this one in the config editor.'"
              @click="choose(e)"
            >
              <img v-if="iconUrl(e.icon)" :src="iconUrl(e.icon)!" alt="" class="wiz-icon" />
              <span v-else class="wiz-icon wiz-icon-fallback" aria-hidden="true">◇</span>
              <span class="wiz-tile-text">
                <b>{{ e.label }}</b>
                <small>{{ e.blurb }}</small>
              </span>
              <span v-if="!e.wizard" class="wiz-soon">config editor</span>
            </button>
          </div>
        </div>
      </div>

      <!-- Stage 2: configure -->
      <div v-else-if="chosen" class="wiz-body">
        <div class="wiz-chosen">
          <img v-if="iconUrl(chosen.icon)" :src="iconUrl(chosen.icon)!" alt="" class="wiz-icon" />
          <div>
            <b>{{ chosen.label }}</b>
            <small>{{ chosen.blurb }}</small>
          </div>
          <button v-if="mode === 'create'" class="btn ghost" @click="stage = 'pick'">
            Change
          </button>
        </div>

        <!-- Connection: the account the ingest step authenticates as. -->
        <section v-if="service" class="wiz-conn">
          <h3 class="wiz-conn-head">Connection</h3>
          <p class="wiz-help wiz-conn-intro">
            Credentials are held by latchkey, under its
            <code>{{ service }}</code> service — datalib never stores them itself.
          </p>

          <label v-if="accountField" class="wiz-field">
            <span class="wiz-label">{{ accountField.label }}</span>
            <span class="wiz-accountrow">
              <!-- A dropdown *and* a box. latchkey may hold an account
                   this server cannot enumerate, and a list that came
                   back empty must not be the only way in. -->
              <select
                class="wiz-input wiz-select wiz-accountpick"
                :value="accounts?.some((a) => a.account === accountValue) ? accountValue : '__other'"
                @change="values[accountField.target] = ($event.target as HTMLSelectElement).value === '__other' ? '' : ($event.target as HTMLSelectElement).value"
              >
                <option value="__other">
                  {{ accounts === null ? "Loading accounts…" : "Type an account…" }}
                </option>
                <option v-for="a in accounts ?? []" :key="a.account || '(default)'" :value="a.account">
                  {{ a.account || "(latchkey’s default account)" }}
                  {{ a.credential_status === "valid" ? "✓" : a.credential_status === "invalid" ? "— expired" : "" }}
                </option>
              </select>
              <input
                class="wiz-input"
                :placeholder="accountField.placeholder"
                :value="values[accountField.target] as string"
                spellcheck="false"
                @input="values[accountField.target] = ($event.target as HTMLInputElement).value"
              />
            </span>
            <small v-if="accountField.help" class="wiz-help">{{ accountField.help }}</small>
            <small v-if="accounts && accounts.length === 0 && !accountsError" class="wiz-help">
              latchkey has no <code>{{ service }}</code> credential stored yet. Connect below.
            </small>
            <small v-if="accountsError" class="wiz-help">
              Couldn’t ask latchkey which accounts it holds ({{ accountsError }}). Type the
              account name — the sync uses latchkey directly and is unaffected by this.
            </small>
          </label>

          <div class="wiz-conn-actions">
            <button
              v-if="canConnect"
              type="button"
              class="btn ghost"
              :disabled="connect.state === 'running'"
              @click="connectViaLatchkey"
            >
              {{ connect.state === "running" ? "Waiting for the browser…" : "Latchkey auth" }}
            </button>
            <button
              v-if="canProbe"
              type="button"
              class="btn ghost"
              :disabled="probe.state === 'running'"
              @click="testConnection"
            >
              {{ probe.state === "running" ? "Testing…" : "Test connection" }}
            </button>
          </div>

          <p
            v-if="canConnect && chosen.credentialConnectWarning"
            class="wiz-help wiz-conn-note"
          >
            {{ chosen.credentialConnectWarning }}
          </p>
          <!-- What the button says on a service that has no browser
               login. Shown rather than done: latchkey refuses to
               re-register a name it holds, so the only way to add one
               destroys the credentials already stored under it. -->
          <div v-if="showConversion" class="wiz-conn-note wiz-convert">
            <p class="wiz-help wiz-convert-head">
              latchkey holds <code>{{ service }}</code> without a browser login, and won’t add one
              to a name it already has. Adding one means taking the service apart and
              registering it again — which <b>deletes every credential stored under
              <code>{{ service }}</code></b>, so it is yours to run, not this dialog’s:
            </p>
            <pre class="wiz-probe-detail">{{ conversionCommands }}</pre>
            <p class="wiz-help">
              Then come back and press <b>Latchkey auth</b>. Or skip all of it and paste a
              credential, below — that needs no conversion and is what this service does today.
            </p>
          </div>

          <!-- A service somebody registered by hand is theirs: latchkey
               refuses to re-register a name, and nothing here should
               want to. Either way, pasting a credential stays available
               and this dialog never takes it away. -->
          <p v-if="canPasteCredential" class="wiz-help wiz-conn-note">
            <template v-if="setOnlyService">
              latchkey holds <code>{{ service }}</code> with no browser login, so a credential is
              stored by hand — which keeps working, and nothing here changes it:
            </template>
            <template v-else>
              A credential can always be pasted instead, one per account, alongside the browser
              login above:
            </template>
            <code>latchkey auth set {{ service }} -H "…"</code>
          </p>
          <p v-if="connect.state !== 'idle'" class="wiz-help wiz-conn-note">
            {{ connect.message }}
          </p>
          <div v-if="probe.state === 'failed'" class="wiz-conn-note wiz-probe-note">
            <p class="wiz-error wiz-probe-headline">{{ probeHeadline }}</p>
            <details v-if="probeDetail">
              <summary class="wiz-help">How to fix it</summary>
              <pre class="wiz-probe-detail">{{ probeDetail }}</pre>
            </details>
          </div>
          <p
            v-else-if="probe.state === 'ok' && probe.report"
            class="wiz-help wiz-conn-note wiz-probe-note"
          >
            Reached
            <b>{{ probe.report.account.address || probe.report.account.id }}</b
            ><!-- A message estimate is only shown when the provider gave
                  one for free: Gmail's profile carries it, JMAP's
                  session does not. --><template
              v-if="probe.report.account.message_estimate"
            >
              — about {{ probe.report.account.message_estimate.toLocaleString() }} messages,
              {{ probe.report.items.length }} {{ probedNoun }}.</template
            ><template v-else>
              — {{ probe.report.items.length }} {{ probedNoun }}.</template
            >
            The pickers below are filled in from it.
          </p>
        </section>

        <label class="wiz-field">
          <span class="wiz-label">Name</span>
          <input
            v-model="name"
            class="wiz-input"
            :placeholder="groupId || '…'"
          />
          <small class="wiz-help">
            What this source is called on screen. Change it whenever you like — nothing on disk
            moves and no step re-runs. Leave it blank to be shown as <code>{{ groupId || "…" }}</code>.
          </small>
        </label>

        <!-- Only while creating. Editing cannot change the id without a
             migration, and a disabled box holding a value you cannot
             alter is a control that exists only to be refused. What it
             was telling you is worth keeping, so Edit says it below as
             the fact it is. -->
        <label v-if="mode === 'create'" class="wiz-field">
          <span class="wiz-label">Id</span>
          <input v-model="id" class="wiz-input" spellcheck="false" @input="idTouched = true" />
          <small class="wiz-help">
            Permanent, and suggested from the name — this is your last chance to change it.
            Creates
            <code>{{ stepIdFor(groupId || "…", "download") }}</code>
            <template v-if="renders">
              and <code>{{ stepIdFor(groupId || "…", "render") }}</code>
            </template>
            under the data root.
          </small>
          <small v-if="idError && idTouched" class="wiz-error">{{ idError }}</small>
        </label>
        <p v-else class="wiz-help wiz-fixed-id">
          Writes under <code>{{ groupId }}/</code> — this source’s folder on disk, and the path the
          search index has already recorded for every document in it, so it can’t change here.
          Use <b>Name</b> above for something you can.
        </p>
        <!-- With no Id field there is nowhere for its validator to
             speak, and `canSubmit` still consults it — so a bad
             inherited id would disable Save with no explanation. -->
        <p v-if="idError && mode !== 'create'" class="wiz-error wiz-fixed-id">{{ idError }}</p>

        <p v-if="missingSteps.length" class="wiz-cred">
          This source is missing
          <template v-for="(step, i) in missingSteps" :key="step"
            ><template v-if="i > 0"> and </template><code>{{ step }}</code></template
          >. Saving writes {{ missingSteps.length === 1 ? "it" : "them" }}.
        </p>
        <p v-if="orphanRender" class="wiz-cred">
          <template v-if="providerRenders">
            Rendering is off below, and this source has a render step,
            <code>{{ orphanRender }}</code
            >.
          </template>
          <template v-else>
            This source has a render step, <code>{{ orphanRender }}</code
            >, but {{ chosen.label }} renders nothing.
          </template>
          Saving removes it, and takes it out of the index steps’ inputs.
        </p>

        <p
          v-if="formFields.length === 0 && renderFields.length === 0 && isEdit"
          class="wiz-help wiz-nofields"
        >
          This source has no options — its id, its name and what it reads are its whole
          configuration.
        </p>

        <template v-for="section in sections" :key="section.key">
          <section v-if="section.heading && providerRenders" class="wiz-section">
            <h3 class="wiz-section-head">{{ section.heading }}</h3>
            <label class="wiz-field wiz-inline">
              <span class="wiz-label">Render this source into markdown</span>
              <input v-model="renderWanted" type="checkbox" class="wiz-bool" />
              <small class="wiz-help">
                A second step, <code>{{ stepIdFor(groupId || "…", "render") }}</code>, turns
                what this brings in into markdown and makes it searchable. It runs on its own and
                can be re-run without fetching anything again. Turn it off and the data is still
                mirrored, but nothing about it reaches the grid or the search index.<template
                  v-if="renderWanted && section.fields.length === 0"
                >
                  It has no settings of its own.</template
                >
              </small>
            </label>
          </section>

          <label
            v-for="f in section.fields"
            :key="f.target"
            class="wiz-field"
            :class="{ 'wiz-inline': INLINE_KINDS.has(f.kind) }"
          >
            <span class="wiz-label">
              {{ f.label }}
              <em v-if="'required' in f && f.required" class="wiz-req">required</em>
            </span>

            <input
              v-if="f.kind === 'bool'"
              type="checkbox"
              class="wiz-bool"
              :checked="!!values[f.target]"
              @change="values[f.target] = ($event.target as HTMLInputElement).checked"
            />
            <select
              v-else-if="f.kind === 'select'"
              class="wiz-input wiz-select"
              :value="values[f.target] as string"
              @change="values[f.target] = ($event.target as HTMLSelectElement).value"
            >
              <option v-for="o in selectOptions(f)" :key="o.value" :value="o.value">
                {{ o.label }}
              </option>
            </select>
            <input
              v-else-if="f.kind === 'date'"
              type="date"
              class="wiz-input"
              :value="values[f.target] as string"
              @input="values[f.target] = ($event.target as HTMLInputElement).value"
            />
            <input
              v-else-if="f.kind === 'int'"
              type="number"
              class="wiz-input wiz-num"
              :value="values[f.target] as string"
              @input="values[f.target] = ($event.target as HTMLInputElement).value"
            />
            <!-- Typed path + native picker. The input stays even in the
                 app: paste is a legitimate way in, and in a browser it is
                 the only one. docs/dev/wizard_file_pickers.md. -->
            <span v-else-if="f.kind === 'path'" class="wiz-pathrow">
              <input
                class="wiz-input wiz-path"
                :placeholder="f.placeholder"
                :value="values[f.target] as string"
                spellcheck="false"
                @input="values[f.target] = ($event.target as HTMLInputElement).value"
              />
              <button
                v-if="canPick"
                type="button"
                class="btn ghost wiz-browse"
                @click="browse(f)"
              >
                {{ f.picks === "file" ? "Choose file…" : "Choose folder…" }}
              </button>
            </span>
            <span v-else-if="f.kind === 'string_list'" class="wiz-listfield">
              <input
                class="wiz-input"
                :placeholder="f.placeholder"
                :value="listText(f)"
                spellcheck="false"
                @input="setListText(f, ($event.target as HTMLInputElement).value)"
              />
              <!-- The picker is an *addition* to the box above, never
                   a replacement: a probe needs credentials that may not
                   exist yet, and this form has to stay usable before one
                   has ever succeeded. Both edit the same array. -->
              <ProbeItemPicker
                v-if="f.probe && probeOptions(f).length"
                :items="probeOptions(f)"
                :model-value="chosenValues(f)"
                @update:model-value="values[f.target] = $event"
              />
              <small v-if="f.probe && unknownValues(f).length" class="wiz-error">
                Not on this account: {{ unknownValues(f).join(", ") }}. A download filter naming a
                label the account doesn’t have fails the run; a render filter naming one renders
                nothing.
              </small>
              <small v-else-if="f.probe && !probe.report" class="wiz-help">
                Run “Test connection” to pick from this account’s real
                {{ PROBE_NOUNS[f.probe] }} instead of typing them.
              </small>
            </span>
            <input
              v-else
              class="wiz-input"
              :placeholder="f.placeholder"
              :value="values[f.target] as string"
              spellcheck="false"
              @input="values[f.target] = ($event.target as HTMLInputElement).value"
            />

            <small v-if="f.help" class="wiz-help">{{ f.help }}</small>
            <small v-if="pickFailed[f.target]" class="wiz-error">
              Couldn’t open the file picker ({{ pickFailed[f.target] }}). Type or paste the path
              instead.
            </small>
          </label>
        </template>

        <details class="wiz-review">
          <summary>Review the TOML this writes</summary>
          <pre>{{ preview }}</pre>
        </details>
      </div>

      <footer class="wiz-foot">
        <span v-if="stage === 'configure' && missingRequired.length" class="wiz-foot-note">
          Still needed: {{ missingRequired.join(", ") }}
        </span>
        <button class="btn ghost" @click="emit('close')">Cancel</button>
        <button
          v-if="stage === 'configure'"
          class="btn primary"
          :disabled="!canSubmit"
          @click="submit"
        >
          {{ isEdit ? "Save changes" : "Add source" }}
        </button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.wiz-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: flex-start;
  justify-content: center;
  padding: 6vh 16px;
  z-index: 50;
}
.wiz {
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 8px;
  width: min(760px, 100%);
  max-height: 88vh;
  display: flex;
  flex-direction: column;
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.35);
}
.wiz-head,
.wiz-foot {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 18px;
}
.wiz-head { border-bottom: 1px solid var(--datalib-border); }
.wiz-foot { border-top: 1px solid var(--datalib-border); justify-content: flex-end; }
.wiz-head h2 { margin: 0; font-size: 17px; flex: 1; }
.wiz-x {
  background: none;
  border: none;
  color: var(--datalib-muted);
  font-size: 22px;
  line-height: 1;
  cursor: pointer;
}
.wiz-body { padding: 16px 18px; overflow-y: auto; }

.wiz-filter,
.wiz-input {
  width: 100%;
  padding: 8px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
}
.wiz-filter { margin-bottom: 16px; }
/* A bool field's own checkbox, sized as a box rather than stretched to
   the field's width like a text input. */
.wiz-bool { width: 16px; height: 16px; }
/* Wide enough for the counts anyone types here, instead of stretching
   across the dialog the way a text field does. */
.wiz-num { width: 7em; }
/* Shares `.wiz-input`'s box; keeps the platform disclosure arrow so it
   doesn't read as a text field you can type into. */
.wiz-select { cursor: pointer; }

.wiz-group h3 {
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--datalib-muted);
  margin: 16px 0 8px;
}
.wiz-tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 8px; }
.wiz-tile {
  display: flex;
  align-items: center;
  gap: 10px;
  text-align: left;
  padding: 10px;
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
  background: var(--datalib-card-bg);
  color: inherit;
  cursor: pointer;
  font: inherit;
}
.wiz-tile:hover:not(:disabled) { background: var(--datalib-hover); }
.wiz-tile.cursor { outline: 2px solid var(--datalib-accent); outline-offset: -1px; }
.wiz-tile.soon { opacity: 0.55; cursor: not-allowed; }
.wiz-tile-text { display: flex; flex-direction: column; min-width: 0; flex: 1; }
.wiz-tile-text b { font-size: 14px; }
.wiz-tile-text small { color: var(--datalib-muted); font-size: 11.5px; }
.wiz-soon {
  font-size: 10px;
  color: var(--datalib-muted);
  border: 1px solid var(--datalib-border);
  border-radius: 3px;
  padding: 1px 4px;
  white-space: nowrap;
}
.wiz-icon { width: 22px; height: 22px; flex: none; }
.wiz-icon-fallback { color: var(--datalib-muted); font-size: 18px; text-align: center; }

.wiz-chosen {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px;
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
  background: var(--datalib-card-bg);
  margin-bottom: 14px;
}
.wiz-chosen div { flex: 1; display: flex; flex-direction: column; }
.wiz-chosen small { color: var(--datalib-muted); font-size: 11.5px; }

.wiz-cred {
  font-size: 12.5px;
  color: var(--datalib-muted);
  border-left: 3px solid var(--datalib-border);
  padding-left: 10px;
  margin: 0 0 16px;
}

.wiz-field { display: flex; flex-direction: column; gap: 4px; margin-bottom: 16px; }
/* Label and control on one line, with the help text wrapping to its own
   full-width row beneath them. */
.wiz-field.wiz-inline { flex-direction: row; flex-wrap: wrap; align-items: center; gap: 4px 8px; }
.wiz-field.wiz-inline .wiz-help,
.wiz-field.wiz-inline .wiz-error { flex: 1 0 100%; }
/* A tickbox reads as "[x] thing", not "thing [x]". */
.wiz-field.wiz-inline .wiz-bool { order: -1; }
.wiz-label { font-size: 12.5px; font-weight: 600; }
.wiz-nofields { margin: 0 0 16px; }
/* The id where it is a fact rather than a field, and the id error that
   then has nowhere else to go. Both sit in the form's flow. */
.wiz-fixed-id { margin: 0 0 16px; }
.wiz-help { color: var(--datalib-muted); font-size: 11.5px; line-height: 1.45; }
.wiz-error { color: #b8481a; font-size: 11.5px; }
.wiz-probe-headline { margin: 0 0 4px; }
/* The step's recipe, in the shape it was written: numbered steps and
   shell commands, which reflowed into a paragraph are unreadable. */
.wiz-probe-detail {
  margin: 6px 0 0;
  padding: 8px 10px;
  max-height: 220px;
  overflow: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  background: var(--datalib-code-bg);
  border-radius: 5px;
  font-size: 11px;
  line-height: 1.5;
}
.wiz-probe-note details > summary { cursor: pointer; }
.wiz-convert {
  border-left: 3px solid var(--datalib-border);
  padding-left: 10px;
}
.wiz-convert-head { margin: 0; }
.wiz-req {
  font-style: normal;
  font-weight: 400;
  font-size: 10.5px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--datalib-muted);
  margin-left: 6px;
}
.wiz-path { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12.5px; }
/* The input takes the slack so the button keeps its label on one line. */
.wiz-pathrow { display: flex; gap: 8px; align-items: center; }
.wiz-pathrow .wiz-input { flex: 1; min-width: 0; }
.wiz-browse { white-space: nowrap; }
.wiz-foot-note { margin-right: auto; font-size: 12px; color: var(--datalib-muted); }

/* The Rendering heading: a rule and a small-caps title, so the render
   step's settings read as a second part of one form rather than a
   second form. */
.wiz-section {
  border-top: 1px solid var(--datalib-border);
  padding-top: 14px;
  margin: 20px 0 12px;
}
.wiz-section-head,
.wiz-conn-head {
  margin: 0 0 6px;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--datalib-muted);
}
/* The section's own toggle sits flush under its heading. */
.wiz-section > .wiz-field { margin-bottom: 0; }

/* The Connection block: latchkey account + the two buttons. Boxed
   because it is about the *account*, not about one setting — the
   fields below it are all things you type, and this is the one place
   that talks to something outside. */
.wiz-conn {
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
  padding: 12px 14px 4px;
  margin-bottom: 16px;
}
.wiz-conn-intro { margin: 0 0 12px; }
.wiz-conn-actions { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }
.wiz-conn-note { margin: 0 0 10px; }
/* Dropdown over box, not side by side: an account is an email address
   and both halves need the width. */
.wiz-accountrow { display: flex; flex-direction: column; gap: 6px; }
.wiz-accountpick { max-width: 100%; }

.wiz-listfield { display: flex; flex-direction: column; gap: 6px; }
.wiz-review { margin-top: 8px; }
.wiz-review summary { cursor: pointer; font-size: 12.5px; color: var(--datalib-muted); }
.wiz-review pre {
  margin: 8px 0 0;
  padding: 10px;
  background: var(--datalib-code-bg);
  border-radius: 5px;
  overflow-x: auto;
  font-size: 12px;
}

.btn {
  padding: 7px 14px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-card-bg);
  color: inherit;
  font: inherit;
  cursor: pointer;
}
.btn:hover:not(:disabled) { background: var(--datalib-hover); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn.primary { background: var(--datalib-accent); border-color: var(--datalib-accent); color: #fff; }
.btn.ghost { background: none; }
</style>
