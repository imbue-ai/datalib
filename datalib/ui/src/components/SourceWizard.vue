<script setup lang="ts">
// The "Add source" / "Edit" dialog: pick a type, fill one form,
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
// block: which latchkey account to use, the "Web login" tab, which runs
// latchkey's browser login, the "Paste a key" tab, which stores a token
// or app password with `latchkey auth set`, and "Check connection", which
// asks the provider's own probe (`datalib-step probe <type>`) which account
// the credentials reach — a green tick that names the account. Each
// `probe:` field has a "Load" of its own that fills its checklist from the
// live account, with progress while it pages. A label picker built from the
// live account is the difference between a filter that works and a filter
// that is a spelling test.
import { computed, onUnmounted, ref, watch } from "vue";
import {
  CATALOG,
  KIND_LABELS,
  entryKey,
  filterCatalog,
  type CatalogEntry,
  type Field,
  type ProbeNoun,
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
  type QmdIndexing,
  type SourceSteps,
} from "@/config/sourceSteps";
import { FailureError } from "@/apiError";
import {
  type AccountNaming,
  type Failure,
  type ConnectPhase,
  type ProbeItem,
  type ProbeItemKind,
  type ProbeProgress,
  type ProbeReport,
  type StoredAccount,
} from "@/api";
import { type SignInWhere } from "@/config/issues";
import { countOf, loadingText } from "@/config/probeProgress";
import { useApi } from "@/cards/cardApi";
import AccountCombo, { type AccountOption } from "@/components/AccountCombo.vue";
import { iconUrl } from "@/config/icons";
import {
  SECRET,
  credentialShape,
  pastedCredential,
  pasteTarget,
  suggestedAccount,
} from "@/config/credentialShape";
import { ingestReach } from "@/config/ingestMethods";
import { loginAccount, nameLeftToService } from "@/config/accountNaming";
import { isDesktopApp, pickPath } from "@/desktop";
import {
  BYTE_UNITS,
  DEFAULT_BYTE_UNIT,
  UNIT_BYTES,
  parseByteSize,
  splitBytes,
  type ByteUnit,
} from "@/config/byteSize";
import IssueNote from "@/components/IssueNote.vue";
import ProbeItemPicker from "@/components/ProbeItemPicker.vue";
import { PATH_GLYPHS, STATUS_GLYPHS } from "@/config/glyphs";
import { copyToClipboard } from "@/clipboard";

const {
  latchkeyService,
  runProbe,
  setLatchkeyCredential,
  startLatchkeyConnect,
  latchkeyConnectStatus,
} = useApi();

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
    /// Which of its own qmd steps this source has today — the config's
    /// way of saying how free-text search reaches it.
    qmdIndexing: QmdIndexing;
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
      /// The group's description as typed, written the same two ways.
      description: string;
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
      /// Which of its `keyword_index` and `embed` steps the source gets.
      /// `none` leaves the markdown out of free-text search; the grid
      /// index is not a choice.
      qmdIndexing: QmdIndexing;
    },
  ): void;
}>();

type Stage = "pick" | "configure";

const mode = computed<"create" | "edit">(() => (props.editing ? "edit" : "create"));
const isEdit = computed(() => mode.value === "edit");

const stage = ref<Stage>(props.editing ? "configure" : "pick");
const query = ref("");
const chosen = ref<CatalogEntry | null>(props.editing?.entry ?? null);

/// Blank means "no name" — a group with none is shown by its id, and
/// clearing the box removes the key. Nothing is ever pre-filled here.
const name = ref(props.editing?.group.name ?? "");
/// What the source is to this person. Blank removes the key, like the
/// name.
const description = ref(props.editing?.group.description ?? "");
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

/// Whether this source's markdown gets a keyword index, and embeddings
/// on top of it. Both on by default, for the same reason rendering is: a
/// source nobody can search is a surprise, not a saving. Editing seeds
/// them from the qmd steps the source has.
const keywordWanted = ref(props.editing ? props.editing.qmdIndexing !== "none" : true);
const embedWanted = ref(props.editing ? props.editing.qmdIndexing === "keyword_and_embed" : true);

/// How far into qmd this source goes: only as far as there is markdown to
/// index, and embeddings only on top of a keyword index.
const qmdIndexing = computed<QmdIndexing>(() => {
  if (!renders.value || !keywordWanted.value) return "none";
  return embedWanted.value ? "keyword_and_embed" : "keyword";
});

/// The fields the form shows for one phase: the descriptor's, less any
/// whose gate is shut.
function activeFields(phase: "download" | "render"): Field[] {
  return chosen.value
    ? fieldsFor(chosen.value, phase).filter((f) => fieldIsActive(f, values.value))
    : [];
}

const downloadFields = computed(() => activeFields("download"));
const renderFields = computed(() => (renders.value ? activeFields("render") : []));

/// The ingest fields the main form renders: the descriptor's, less the
/// latchkey account. That one lands on the same params target and is
/// written by the same code as any other field, but it is *shown* in
/// the Connection block beside the buttons that sign in to it; rendering
/// it twice is the bug this exists to prevent.
const formFields = computed(() =>
  downloadFields.value.filter((f) => !(f.kind === "text" && f.latchkey)),
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
const INLINE_KINDS = new Set<Field["kind"]>(["bool", "int", "bytes"]);

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
    .map((kind) => ({
      kind,
      label: KIND_LABELS[kind],
      entries: matches.filter((e) => e.kind === kind),
    }))
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

/// The unit each `bytes` field is shown in. A `bytes` value is the
/// string the config holds ("5 MB"); the unit is read off it when it is
/// one the dropdown offers, else chosen so the number shows whole, and
/// an empty field starts on the default. Kept apart from `values` so an
/// empty field still remembers the unit picked for it.
const byteUnits = ref<Record<string, ByteUnit>>({});

function seed(entry: CatalogEntry, steps?: SourceSteps) {
  values.value = seedFieldValues(entry, steps);
  byteUnits.value = {};
  for (const f of entry.fields ?? []) {
    if (f.kind !== "bytes") continue;
    const parsed = parseByteSize(String(values.value[f.target] ?? ""));
    byteUnits.value[f.target] =
      parsed === null ? DEFAULT_BYTE_UNIT : (parsed.unit ?? splitBytes(parsed.bytes).unit);
  }
}

function byteUnit(f: Field): ByteUnit {
  return byteUnits.value[f.target] ?? DEFAULT_BYTE_UNIT;
}
function byteAmount(f: Field): string {
  const parsed = parseByteSize(String(values.value[f.target] ?? ""));
  return parsed === null ? "" : String(parsed.bytes / UNIT_BYTES[byteUnit(f)]);
}
function setByteAmount(f: Field, text: string) {
  values.value[f.target] = text.trim() === "" ? "" : `${text.trim()} ${byteUnit(f)}`;
}
/// Changing the unit keeps the number — "5 MB" becomes "5 GB", the way a
/// phone's data-limit dialog does it — rather than re-expressing the
/// same bytes in the new unit.
function setByteUnit(f: Field, unit: ByteUnit) {
  const amount = byteAmount(f);
  byteUnits.value[f.target] = unit;
  if (amount !== "") values.value[f.target] = `${amount} ${unit}`;
}

if (props.editing) seed(props.editing.entry, props.editing.steps);

function choose(entry: CatalogEntry) {
  if (!entry.wizard) return;
  chosen.value = entry;
  // No name typed yet, so the id starts from the catalog's default.
  // Typing a name re-derives it, until the id is touched directly.
  id.value = suggestId(props.takenIds, "", entry.defaultName);
  idTouched.value = false;
  seed(entry);
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

/// The example the Name help gives. The id would be the tempting thing
/// to show, since a blank name falls back to it — but the id is a path
/// segment and the name is display text, and "whatsapp" as the example
/// invites a name shaped like an id.
const nameHint = computed(() => chosen.value?.nameHint ?? "…");
const idError = computed(() => {
  const n = groupId.value;
  if (!n) return "An id is required.";
  if (RESERVED.has(n)) return `"${n}" is reserved — it names a directory the pipeline owns.`;
  if (n === "." || n === "..") return "The id must not be '.' or '..'.";
  if (n.startsWith("-")) return "The id must not start with '-'.";
  if (!/^[A-Za-z0-9._-]+$/.test(n))
    return "Use only letters, digits, '.', '_' and '-' — the id becomes a directory.";
  if (mode.value === "create" && props.takenIds.has(n)) return `"${n}" is already configured.`;
  return null;
});

/// Fields the provider's Rust struct declares non-optional — a
/// `PathBuf` rather than an `Option<PathBuf>` — so a config missing one
/// fails at deserialize time rather than at sync time. Caught here so
/// the message lands under the field instead of in a job log.
const missingRequired = computed(() => {
  const fields = [...downloadFields.value, ...renderFields.value];
  const blank = (f: Field) => String(values.value[f.target] ?? "").trim() === "";
  const missing = fields
    .filter((f) => "required" in f && f.required)
    .filter(blank)
    .map((f) => f.label);
  const oneOf = fields.filter((f) => chosen.value?.requiresOneOf?.includes(f.target));
  if (oneOf.length && oneOf.every(blank)) missing.push(oneOf.map((f) => f.label).join(" or "));
  return missing;
});

const canSubmit = computed(() => !idError.value && missingRequired.value.length === 0);

/// What this dialog writes: the group while creating, and the source's
/// steps either way.
const source = computed(() =>
  chosen.value
    ? buildSource({
        entry: chosen.value,
        group: groupId.value,
        name: name.value,
        description: description.value,
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

function parseList(text: string): string[] {
  return text
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}
/// What was typed, kept per field target: re-rendering the parsed array
/// would swallow a trailing comma (or space) the moment it is typed. A
/// draft that no longer parses to the array — the picker changed it —
/// gives way to the array.
const listDrafts = ref<Record<string, string>>({});
function listText(field: Field): string {
  const v = values.value[field.target];
  const items = Array.isArray(v) ? (v as string[]) : [];
  const draft = listDrafts.value[field.target];
  if (draft !== undefined && parseList(draft).join("\0") === items.join("\0")) return draft;
  return items.join(", ");
}
function setListText(field: Field, text: string) {
  listDrafts.value[field.target] = text;
  values.value[field.target] = parseList(text);
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
    startIn: f.startIn,
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

/// A path field's help split around its `startIn`, so the path can
/// carry a copy button: Cmd-Shift-G in the picker, or a terminal, is
/// where it gets pasted.
function helpAroundStart(f: Field): { before: string; path: string; after: string } | null {
  if (f.kind !== "path" || !f.startIn || !f.help) return null;
  const at = f.help.indexOf(f.startIn);
  if (at < 0) return null;
  return {
    before: f.help.slice(0, at),
    path: f.startIn,
    after: f.help.slice(at + f.startIn.length),
  };
}

const copiedStart = ref<string | null>(null);

async function copyStart(f: Field) {
  if (f.kind !== "path" || !f.startIn) return;
  if (await copyToClipboard(f.startIn)) {
    copiedStart.value = f.target;
    setTimeout(() => {
      if (copiedStart.value === f.target) copiedStart.value = null;
    }, 1500);
  }
}

/// Every field is a <label>, so a click anywhere in one activates its
/// control — including the click that ends a drag across the help text,
/// which moves focus to the input and drops the selection. Help text is
/// there to be read and copied from.
function keepHelpSelectable(e: MouseEvent) {
  const t = e.target instanceof Element ? e.target : null;
  if (t?.closest("label .wiz-help") && !t.closest("a, button, input, select, textarea")) {
    e.preventDefault();
  }
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

/// The one field that holds a latchkey account. Every latchkey source
/// has exactly one (`fieldsFor` adds it where the descriptor does not):
/// a step mirrors one identity.
const accountField = computed(
  () =>
    downloadFields.value.find((f) => f.kind === "text" && f.latchkey) as
      (Field & { kind: "text" }) | undefined,
);

const accounts = ref<StoredAccount[] | null>(null);
const authOptions = ref<string[]>([]);
/// Whether latchkey holds this service already. Starts true so nothing
/// offers to register one before the answer is in.
const serviceRegistered = ref(true);
/// How to invoke latchkey on the machine running the backend. `npx`
/// until the server says otherwise, so a command is never shown naming
/// a binary that isn't there.
const latchkeyCli = ref("latchkey");
/// The latchkey gateway the backend talks through, when there is one.
/// Under a gateway the credentials and the browser that signs in to
/// them are on the gateway's side, and latchkey refuses every command
/// the login button would run — so the button is not offered at all.
const gateway = ref<string | null>(null);
/// Why latchkey could not be asked, when it could not — shown at the
/// top of the Connection section for every source, since without the
/// answer it offers no way to sign in at all.
const accountsFailure = ref<Failure | null>(null);
/// Where signing in will install the latchkey plugin this service comes
/// from, while latchkey lacks it.
const installsPlugin = ref<string | null>(null);
/// Who names the account a browser login adds: the service, from who
/// signed in, or the person, in the box.
const accountNaming = ref<AccountNaming>("chosen");
const storedNames = computed(() => (accounts.value ?? []).map((a) => a.account));
/// The box names an account latchkey holds for a service that names its
/// own, so signing in refreshes it rather than adding one.
const signsInAgainAs = computed(() =>
  accountNaming.value === "service" && accountValue.value
    ? loginAccount("service", storedNames.value, accountValue.value)
    : "",
);
const accountHelp = computed(() =>
  accountNaming.value === "service"
    ? `${chosen.value?.label ?? "The service"} names each account itself when you sign in. ` +
      "Pick one latchkey holds, or sign in to add another."
    : accountField.value?.help,
);

async function loadAccounts() {
  const name = service.value;
  if (!name) return;
  accounts.value = null;
  accountsFailure.value = null;
  try {
    const info = await latchkeyService(name);
    accounts.value = info.accounts;
    authOptions.value = info.auth_options;
    setExample.value = info.set_example ?? null;
    serviceRegistered.value = info.registered;
    latchkeyCli.value = info.cli;
    gateway.value = info.gateway;
    installsPlugin.value = info.installs_plugin;
    accountNaming.value = info.account_naming;
    accountsFailure.value = info.error
      ? { issue: info.issue ?? "unknown", detail: info.error }
      : null;
    if (!signInTab.value || !signInWays.value.includes(signInTab.value))
      chooseSignIn(signInWays.value[0] ?? null);
  } catch (e) {
    accounts.value = [];
    accountsFailure.value = toFailure(e);
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
  () =>
    !gateway.value && (authOptions.value.includes("browser") || !!chosen.value?.credentialRegister),
);

/// A service latchkey holds that cannot do a browser login. Its owner
/// set it up by hand, so the dialog says how to add a credential the
/// same way rather than offering to change it. Not under a gateway:
/// `auth set` is refused there too, and the gateway note says where
/// credentials come from instead.
const setOnlyService = computed(
  () => !gateway.value && serviceRegistered.value && !authOptions.value.includes("browser"),
);

/// latchkey's example for this service knows the credential's shape —
/// `fastmail-dav` takes `-u user:password`, not a header. A service
/// registered by hand may have none, and gets the generic header form.
const setExample = ref<string | null>(null);
const setCommand = computed(() => {
  const example = setExample.value ?? `latchkey auth set ${service.value} -H "…"`;
  return example.replace(/^latchkey /, `${latchkeyCli.value} `);
});

// Pasting a credential

/// Offered wherever latchkey takes a credential by hand — every service
/// the wizard names — except under a gateway, which refuses `auth set`.
const canPaste = computed(() => !gateway.value && authOptions.value.includes("set"));
const pasteShape = computed(() =>
  credentialShape(setExample.value, chosen.value?.credentialPaste?.headers),
);
const pasteUsername = ref("");
const pasteSecret = ref("");
/// Set once the account field is typed in or picked from, or already
/// named an account when the paste form opened. Until then the field
/// follows the pasted username, since that is what it is stored under.
const accountChosen = ref(false);
const paste = ref<{
  state: "idle" | "saving" | "ok" | "failed";
  message: string;
  failure?: Failure;
}>({
  state: "idle",
  message: "",
});
/// The ways in this service offers, in the order the tabs show them: a
/// browser login gets whatever the sign-in grants, usually everything; a
/// pasted key can be one made with less, where the service offers that.
type SignInWay = "web" | "paste";
const signInWays = computed<SignInWay[]>(() => [
  ...(canConnect.value ? (["web"] as const) : []),
  ...(canPaste.value ? (["paste"] as const) : []),
]);
const signInTab = ref<SignInWay | null>(null);

function chooseSignIn(way: SignInWay | null) {
  signInTab.value = way;
  if (way === "paste") preparePaste();
}

function preparePaste() {
  accountChosen.value = !!accountValue.value;
  const address = accountValue.value.split(/\s/)[0] ?? "";
  if (!pasteUsername.value && address.includes("@")) pasteUsername.value = address;
}
watch(pasteUsername, (username) => {
  const field = accountField.value;
  if (field && !accountChosen.value)
    values.value[field.target] = suggestedAccount(
      username,
      chosen.value?.credentialPaste?.accountSuffix,
    );
});

/// A folder is imported, not pasted.
const pasteTabLabel = computed(() =>
  pasteShape.value.kind === "directory" ? "Import tokens" : "Paste a key",
);

/// latchkey's own word for the secret: "Token", "App password".
const pasteSecretLabel = computed(() => {
  const label = pasteShape.value.secretLabel;
  return label.charAt(0).toUpperCase() + label.slice(1);
});

const pasted = computed(() =>
  pastedCredential(pasteShape.value, pasteUsername.value, pasteSecret.value),
);

/// The header the secret is sent in, shown with the secret elided so a
/// person can see it is the kind of thing they have.
const pasteHeaderHint = computed(() =>
  pasteShape.value.kind === "headers" && pasteShape.value.headers[0] !== SECRET
    ? pasteShape.value.headers.map((h) => h.replace(SECRET, "…")).join("  ")
    : "",
);

/// Where the source names an account, the paste is stored under that
/// name and must have one: an unnamed paste lands on whichever
/// credential latchkey holds alone.
const pasteNeedsName = computed(() => !!accountField.value);
const pasteLandsOn = computed(() =>
  pasteTarget(
    (accounts.value ?? []).map((a) => a.account),
    accountValue.value,
  ),
);

async function savePasted() {
  const name = service.value;
  const credential = pasted.value;
  if (!name || !credential || paste.value.state === "saving") return;
  const account = pasteNeedsName.value ? accountValue.value : "";
  if (pasteNeedsName.value && !account) return;
  paste.value = { state: "saving", message: "" };
  try {
    await setLatchkeyCredential(name, account, credential);
  } catch (e) {
    paste.value = { state: "failed", message: "", failure: toFailure(e) };
    return;
  }
  pasteSecret.value = "";
  paste.value = { state: "ok", message: "Stored in latchkey." };
  await loadAccounts();
  // The credential is only a guess until something uses it; the check
  // is the cheapest thing that does.
  resetProbes();
  if (canProbe.value) await checkConnection();
}

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

const connect = ref<{
  state: "idle" | "running" | "ok" | "failed";
  message: string;
  phase?: ConnectPhase;
  failure?: Failure;
}>({
  state: "idle",
  message: "",
});

/// Set on unmount so an in-flight poll stops rather than writing into a
/// dialog that is gone.
let closed = false;
onUnmounted(() => {
  closed = true;
});

/// What a running login says it is waiting on.
const CONNECT_PHASE_TEXT: Record<ConnectPhase, string> = {
  preparing: "Getting the sign-in ready…",
  downloading_browser:
    "Getting a browser for the sign-in — a one-time download that can take a few minutes…",
  signing_in: "A browser window should open. Finish the login there.",
};

async function connectViaLatchkey() {
  const name = service.value;
  if (!name || connect.value.state === "running") return;
  if (setOnlyService.value) {
    showConversion.value = true;
    return;
  }
  connect.value = {
    state: "running",
    message: "A browser window should open. Finish the login there.",
  };
  try {
    // A cookie capture has to watch a real sign-in happen. latchkey
    // otherwise restores the session it saved last time, so the browser
    // opens already signed in, no new cookie is issued, and the login
    // waits forever (imbue-ai/latchkey#150). Told from the descriptor,
    // not from whether we are registering: the flow is the service's
    // either way.
    const ephemeral = chosen.value?.credentialRegister?.login_flow === "cookie-capture";
    const started = await startLatchkeyConnect(
      name,
      loginAccount(accountNaming.value, storedNames.value, accountValue.value),
      wouldRegister.value,
      ephemeral,
    );
    for (;;) {
      await new Promise((r) => setTimeout(r, 1500));
      if (closed) return;
      const status = await latchkeyConnectStatus(started.id);
      if (status.status === "running") {
        connect.value = {
          state: "running",
          message: CONNECT_PHASE_TEXT[status.phase],
          phase: status.phase,
        };
        continue;
      }
      if (status.status === "ok") {
        // The point of connecting was to add an account; showing the
        // stale list would hide the one just added.
        await loadAccounts();
        // Follow the login rather than the box: a service that names
        // its own accounts files the credential under whoever signed
        // in, and the config has to name that one.
        const landed = status.account;
        const field = accountField.value;
        if (landed && field) values.value[field.target] = landed;
        // What was checked and loaded before the login was done with a
        // credential that has just been replaced.
        resetProbes();
        connect.value = {
          state: "ok",
          message: landed
            ? `Connected as ${landed}.`
            : "Connected. The account list below is refreshed.",
        };
      } else {
        connect.value = {
          state: "failed",
          message: "",
          failure: {
            issue: status.issue ?? "unknown",
            detail: status.output || "The login did not complete.",
          },
        };
      }
      return;
    }
  } catch (e) {
    connect.value = { state: "failed", message: "", failure: toFailure(e) };
  }
}

/// The names latchkey holds for this service, with what it says of each.
/// Its status is a hint, not a verdict: `fastmail-dav` checks every
/// credential against CardDAV, so a calendar-only password reads invalid.
const accountOptions = computed<AccountOption[] | null>(() =>
  accounts.value === null
    ? null
    : accounts.value.map((a) => ({
        value: a.account,
        note:
          a.credential_status === "valid"
            ? "✓"
            : a.credential_status === "invalid"
              ? "latchkey reports it invalid"
              : undefined,
      })),
);

function chooseAccount(name: string) {
  const field = accountField.value;
  if (!field) return;
  values.value[field.target] = name;
  accountChosen.value = true;
}

/// The account currently in the form. Empty means "latchkey's unnamed
/// default", which is addressed by writing no `account` at all — so
/// empty is a real answer, not a missing one.
const accountValue = computed(() =>
  accountField.value ? String(values.value[accountField.value.target] ?? "").trim() : "",
);

// The probe: "Check connection" asks which account the credentials
// reach; each picker's "Load" asks for its own list, which can take a
// while on a big account, so it says how far it has got.

type Outcome = { state: "idle" | "running" | "ok" | "failed"; failure: Failure | null };

const check = ref<Outcome & { report: ProbeReport | null }>({
  state: "idle",
  failure: null,
  report: null,
});

type ListLoad = Outcome & {
  report: ProbeReport | null;
  progress: ProbeProgress | null;
  startedAt: number;
};

const IDLE_LIST: ListLoad = {
  state: "idle",
  failure: null,
  report: null,
  progress: null,
  startedAt: 0,
};

/// One load per list, shared by every field that picks from it.
const lists = ref<Partial<Record<ProbeNoun, ListLoad>>>({});

function listLoad(noun: ProbeNoun): ListLoad {
  return lists.value[noun] ?? IDLE_LIST;
}

/// Forget what was checked and loaded: the credentials it was done
/// with have just changed.
function resetProbes() {
  check.value = { state: "idle", failure: null, report: null };
  lists.value = {};
}

/// Can "Check connection" and the pickers' "Load" be offered here?
const canProbe = computed(() => !!chosen.value?.canProbe && !!probeParams.value);

async function checkConnection() {
  const entry = chosen.value;
  const params = probeParams.value;
  if (!entry || !params || check.value.state === "running") return;
  check.value = { state: "running", failure: null, report: null };
  try {
    const report = await runProbe(entry.type, params, null);
    check.value = { state: "ok", failure: null, report };
  } catch (e) {
    check.value = { state: "failed", failure: toFailure(e), report: null };
  }
}

async function loadList(noun: ProbeNoun) {
  const entry = chosen.value;
  const params = probeParams.value;
  if (!entry || !params || listLoad(noun).state === "running") return;
  lists.value[noun] = { ...IDLE_LIST, state: "running", startedAt: Date.now() };
  tickWhileLoading();
  try {
    const report = await runProbe(entry.type, params, noun, (progress) => {
      const load = lists.value[noun];
      if (load) load.progress = progress;
    });
    lists.value[noun] = { ...listLoad(noun), state: "ok", report };
  } catch (e) {
    lists.value[noun] = { ...listLoad(noun), state: "failed", failure: toFailure(e) };
  }
}

/// The clock the loading lines read their seconds from, ticking only
/// while something loads.
const now = ref(Date.now());
let ticker: ReturnType<typeof setInterval> | null = null;

function tickWhileLoading() {
  if (ticker) return;
  ticker = setInterval(() => {
    now.value = Date.now();
    if (!Object.values(lists.value).some((l) => l?.state === "running")) {
      clearInterval(ticker as ReturnType<typeof setInterval>);
      ticker = null;
    }
  }, 1000);
}

onUnmounted(() => {
  if (ticker) clearInterval(ticker);
});

// A check or a list is about the account it was made with.
watch(accountValue, resetProbes);

function loadingLine(noun: ProbeNoun): string {
  const load = listLoad(noun);
  return loadingText(probeNoun(noun), load.progress, (now.value - load.startedAt) / 1000);
}

/// Any failure as the wizard shows one: a classified one as it came,
/// anything else (a dropped connection to this server) as its text.
function toFailure(e: unknown): Failure {
  if (e instanceof FailureError) return e.failure;
  return { issue: "unknown", detail: e instanceof Error ? e.message : String(e) };
}

/// Where this dialog can get a credential into latchkey from, which is
/// what a failure's advice points at.
const signInWhere = computed<SignInWhere>(() =>
  gateway.value ? "gateway" : canConnect.value || canPaste.value ? "here" : "terminal",
);

/// Which of a report's item kinds each `probe:` noun takes. A render
/// filter matches only what emails are filed in, never a Gmail flag,
/// which is why `mailboxes` is narrower than `labels`.
const PROBE_KINDS: Record<ProbeNoun, ProbeItemKind[]> = {
  labels: ["mailbox", "keyword"],
  mailboxes: ["mailbox"],
  conversations: ["conversation"],
  channels: ["channel"],
  calendars: ["calendar"],
  addressbooks: ["address_book"],
};

/// What a `probe:` field should offer, given what its list loaded.
function probeOptions(field: Field): ProbeItem[] {
  if (field.kind !== "string_list" || !field.probe) return [];
  const report = listLoad(field.probe).report;
  if (!report) return [];
  const kinds = PROBE_KINDS[field.probe];
  return report.items.filter((i) => kinds.includes(i.kind));
}

/// What the field holds today, as the array the picker binds to.
function chosenValues(field: Field): string[] {
  const v = values.value[field.target];
  return Array.isArray(v) ? (v as string[]) : [];
}

/// Does a typed value name this item, the way the provider will read
/// it? Exact on the path, except where the downloader itself is looser:
/// Slack drops a leading `#` from a channel name, and every chat source
/// takes a pasted link to a conversation as well as its bare id — so a
/// value that *ends* in the id, after a `/`, counts.
function namesItem(value: string, item: ProbeItem): boolean {
  const v = value.trim();
  if (v === item.path) return true;
  switch (item.kind) {
    case "channel":
      return v.replace(/^#/, "") === item.path;
    case "conversation":
      return v.split(/[?#]/)[0]?.split("/").includes(item.path) ?? false;
    // The `calendars` filter matches a name without regard to case, and
    // an id as well; the id is what the probe puts in `title`.
    case "calendar":
      return v.toLowerCase() === item.path.toLowerCase() || v === item.title;
    default:
      return false;
  }
}

/// Chosen values the probed account does not have.
function unknownValues(field: Field): string[] {
  const options = probeOptions(field);
  if (options.length === 0) return [];
  return chosenValues(field).filter((v) => !options.some((i) => namesItem(v, i)));
}

/// What a field's picker is a picker *of*, for the sentences around it.
/// A mailbox goes by the source's own word — Gmail's "labels", a JMAP
/// server's "folders" — since the other word reads as a bug; a Claude
/// account has conversations and a Slack workspace channels.
function probeNoun(probe: ProbeNoun): string {
  if (probe === "labels" || probe === "mailboxes") return chosen.value?.mailboxNoun ?? "folders";
  if (probe === "addressbooks") return "address books";
  return probe;
}

/// Who a report reached, in the words the wizard shows it by.
function reachedName(report: ProbeReport): string {
  return report.account.address || report.account.display_name || report.account.id;
}

/// "12 channels from picard in Enterprise" — what a load came back
/// with, counted in the field's own noun.
function loadedLine(field: Field): string {
  if (field.kind !== "string_list" || !field.probe) return "";
  const report = listLoad(field.probe).report;
  if (!report) return "";
  const n = probeOptions(field).length;
  return n === 0
    ? `No ${probeNoun(field.probe)} on ${reachedName(report)}.`
    : `${countOf(n, probeNoun(field.probe))} from ${reachedName(report)}.`;
}

// Load the account list as soon as there is a service to load it for:
// on open in edit mode, and on picking a tile in create mode.
watch(
  service,
  (name) => {
    // A half-typed secret belongs to the service it was typed for, and
    // a check or a list to the source it was made for.
    resetProbes();
    signInTab.value = null;
    pasteSecret.value = "";
    paste.value = { state: "idle", message: "" };
    if (name) void loadAccounts();
  },
  { immediate: true },
);

function submit() {
  if (!canSubmit.value || !chosen.value || !source.value) return;
  emit("submit", {
    id: groupId.value,
    name: name.value.trim(),
    description: description.value.trim(),
    entry: chosen.value,
    groupBody: source.value.groupBody,
    stepsBody: source.value.stepsBody,
    renderId: source.value.renderId,
    qmdIndexing: qmdIndexing.value,
  });
}
</script>

<template>
  <!-- The backdrop deliberately does not close this dialog: a stray
       click beside a half-filled form would discard every field in it
       with nothing to undo it. The × and Cancel are the ways out. -->
  <div class="wiz-backdrop dialog-backdrop">
    <div
      class="wiz dialog"
      role="dialog"
      aria-modal="true"
      :aria-label="isEdit ? 'Edit source' : 'Add data source'"
      @click="keepHelpSelectable"
    >
      <header class="wiz-head dialog-head">
        <h2>{{ isEdit ? `Edit ${name || id}` : "Add a data source" }}</h2>
        <button class="wiz-x dialog-x" aria-label="Close" @click="emit('close')">×</button>
      </header>

      <!-- Stage 1: pick a type -->
      <div v-if="stage === 'pick'" class="wiz-body dialog-body">
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
              :title="
                e.wizard ? e.blurb : 'No guided setup yet — add this one in the config editor.'
              "
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
      <div v-else-if="chosen" class="wiz-body dialog-body">
        <div class="wiz-chosen">
          <img v-if="iconUrl(chosen.icon)" :src="iconUrl(chosen.icon)!" alt="" class="wiz-icon" />
          <div>
            <b>{{ chosen.label }}</b>
            <small>{{ chosen.blurb }}</small>
          </div>
          <button v-if="mode === 'create'" class="btn ghost" @click="stage = 'pick'">Change</button>
        </div>

        <!-- Connection: the account the ingest step authenticates as. -->
        <section v-if="service" class="wiz-conn">
          <h3 class="wiz-conn-head">Connection</h3>
          <p class="wiz-help wiz-conn-intro">
            Credentials are held by latchkey, under its
            <code>{{ service }}</code> service — datalib never stores them itself.
          </p>
          <!-- Every way to sign in below comes from latchkey's answer, so
               while it is asked the section says so, and when it cannot
               be asked the section says why — for every source, not only
               the ones with an account picker. -->
          <p v-if="accounts === null" class="wiz-help wiz-conn-asking" role="status">
            Asking latchkey how you can sign in…
          </p>
          <IssueNote
            v-if="accountsFailure"
            class="wiz-conn-note wiz-accounts-failed"
            :failure="accountsFailure"
            :service="chosen.label"
            :where="signInWhere"
          />

          <div v-if="accountField" class="wiz-field">
            <span class="wiz-label">{{ accountField.label }}</span>
            <AccountCombo
              :model-value="accountValue"
              :options="accountOptions"
              :label="accountField.label"
              :placeholder="
                accountNaming === 'service' ? 'named when you sign in' : 'you@example.com'
              "
              @update:model-value="chooseAccount"
            />
            <small v-if="accountHelp" class="wiz-help wiz-account-help">{{ accountHelp }}</small>
            <small v-if="accounts && accounts.length === 0 && !accountsFailure" class="wiz-help">
              latchkey has no <code>{{ service }}</code> credential stored yet.
              {{ canConnect ? "Connect below." : "" }}
            </small>
          </div>

          <!-- Under a gateway the login happens on the gateway's side,
               and every command a sign-in would run is refused. -->
          <p v-if="gateway" class="wiz-help wiz-conn-note">
            Credentials are held by a latchkey gateway (<code>{{ gateway }}</code
            >). Sign in where that gateway is managed, then press <b>Check connection</b>.
          </p>
          <p
            v-if="installsPlugin && signInWays.length"
            class="wiz-help wiz-conn-note wiz-plugin-note"
          >
            latchkey reaches {{ chosen.label }} through a plugin. Signing in installs it into
            <code>{{ installsPlugin }}</code
            >.
          </p>
          <!-- How a credential gets into latchkey under the name above. A
               tab per way the service offers; a lone way is shown bare. -->
          <div v-if="signInWays.length" class="wiz-signin">
            <div v-if="signInWays.length > 1" class="wiz-tabs" role="tablist">
              <button
                v-for="way in signInWays"
                :id="`wiz-tab-${way}`"
                :key="way"
                type="button"
                role="tab"
                class="wiz-tab"
                :aria-selected="signInTab === way"
                aria-controls="wiz-signin-panel"
                @click="chooseSignIn(way)"
              >
                {{ way === "web" ? "Web login" : pasteTabLabel }}
              </button>
            </div>
            <div
              v-if="signInTab === 'web'"
              id="wiz-signin-panel"
              class="wiz-tabpanel"
              :role="signInWays.length > 1 ? 'tabpanel' : undefined"
              :aria-labelledby="signInWays.length > 1 ? 'wiz-tab-web' : undefined"
            >
              <p class="wiz-help">
                Opens a browser window to sign in. latchkey keeps what the sign-in grants, which is
                usually full access: it can read and change everything the account can.
                <template v-if="signInWays.includes('paste') && pasteShape.kind !== 'directory'"
                  >For less, use <b>Paste a key</b>.</template
                >
              </p>
              <p
                v-if="nameLeftToService(accountNaming, storedNames, accountValue)"
                class="wiz-help wiz-name-left"
              >
                {{ chosen.label }} names the new account itself, so
                <code>{{ accountValue }}</code> is replaced by the name it reports.
              </p>
              <p v-if="chosen.credentialConnectWarning" class="wiz-help">
                {{ chosen.credentialConnectWarning }}
              </p>
              <div class="wiz-conn-actions">
                <button
                  type="button"
                  class="btn"
                  :disabled="connect.state === 'running'"
                  @click="connectViaLatchkey"
                >
                  {{
                    connect.state !== "running"
                      ? signsInAgainAs
                        ? `Sign in again as ${signsInAgainAs}`
                        : "Sign in with browser"
                      : connect.phase === "downloading_browser"
                        ? "Getting a browser…"
                        : "Waiting for the browser…"
                  }}
                </button>
              </div>
              <IssueNote
                v-if="connect.state === 'failed' && connect.failure"
                class="wiz-connect-failed"
                :failure="connect.failure"
                :service="chosen.label"
                :where="signInWhere"
              />
              <p v-else-if="connect.state !== 'idle'" class="wiz-help wiz-connect-status">
                {{ connect.message }}
              </p>
              <!-- What the button says on a service that has no browser
                   login. Shown rather than done: latchkey refuses to
                   re-register a name it holds, so the only way to add one
                   destroys the credentials already stored under it. -->
              <div v-if="showConversion" class="wiz-conn-note wiz-convert">
                <p class="wiz-help wiz-convert-head">
                  latchkey holds <code>{{ service }}</code> without a browser login, and won’t add
                  one to a name it already has. Adding one means taking the service apart and
                  registering it again — which
                  <b
                    >deletes every credential stored under <code>{{ service }}</code></b
                  >, so it is yours to run, not this dialog’s:
                </p>
                <pre class="wiz-probe-detail">{{ conversionCommands }}</pre>
                <p class="wiz-help">
                  Then come back and press <b>Sign in with browser</b>. Or skip all of it and use
                  the <b>Paste a key</b> tab — that needs no conversion and is what this service
                  does today.
                </p>
              </div>
            </div>
            <!-- latchkey's `auth set`, run by the server. The secret is
                 sent once and never kept in the form after it is stored. -->
            <div
              v-else-if="signInTab === 'paste'"
              id="wiz-signin-panel"
              class="wiz-tabpanel wiz-paste"
              :role="signInWays.length > 1 ? 'tabpanel' : undefined"
              :aria-labelledby="signInWays.length > 1 ? 'wiz-tab-paste' : undefined"
            >
              <p v-if="!signInWays.includes('web')" class="wiz-help">
                <code>{{ service }}</code> has no web login, so its credential is pasted here.
              </p>
              <p v-if="chosen.credentialPaste?.help" class="wiz-help">
                {{ chosen.credentialPaste.help }}
              </p>
              <label v-if="pasteShape.kind === 'basic'" class="wiz-field">
                <span class="wiz-label">Username</span>
                <input
                  v-model="pasteUsername"
                  class="wiz-input"
                  :placeholder="pasteShape.userHint"
                  autocomplete="off"
                  spellcheck="false"
                />
              </label>
              <label class="wiz-field">
                <span class="wiz-label">{{ pasteSecretLabel }}</span>
                <input
                  v-model="pasteSecret"
                  class="wiz-input"
                  :type="pasteShape.kind === 'directory' ? 'text' : 'password'"
                  :placeholder="
                    pasteShape.kind === 'directory' ? pasteShape.placeholder : undefined
                  "
                  autocomplete="off"
                  spellcheck="false"
                />
                <small v-if="pasteHeaderHint" class="wiz-help">
                  Sent as <code>{{ pasteHeaderHint }}</code>
                </small>
              </label>
              <p v-if="pasteNeedsName" class="wiz-help">
                <template v-if="pasteLandsOn.kind === 'unnamed'">
                  Name the {{ accountField?.label ?? "account" }} above: latchkey stores this
                  credential under that name.
                </template>
                <template v-else-if="pasteLandsOn.kind === 'replaces'">
                  Stored as <code>{{ pasteLandsOn.account }}</code
                  >, replacing the <code>{{ service }}</code> credential latchkey already holds
                  under that name — every source that uses it gets this one. Choose another name
                  above to keep it.
                </template>
                <template v-else>
                  Stored as <code>{{ accountValue }}</code
                  >.
                  <template v-if="pasteLandsOn.besideUnnamed">
                    latchkey also holds an unnamed <code>{{ service }}</code> credential; with both
                    stored, it won’t pick one for a source that names no account, so name an account
                    in those sources too.
                  </template>
                </template>
              </p>
              <div class="wiz-conn-actions">
                <button
                  type="button"
                  class="btn"
                  :disabled="
                    !pasted ||
                    (pasteNeedsName && pasteLandsOn.kind === 'unnamed') ||
                    paste.state === 'saving'
                  "
                  @click="savePasted"
                >
                  {{ paste.state === "saving" ? "Storing…" : "Store in latchkey" }}
                </button>
              </div>
              <IssueNote
                v-if="paste.state === 'failed' && paste.failure"
                :failure="paste.failure"
                :service="chosen.label"
                :where="signInWhere"
              />
              <p v-else-if="paste.message" class="wiz-help">
                {{ paste.message }}
              </p>
              <p class="wiz-help">
                From a terminal instead: <code>{{ setCommand }}</code>
              </p>
            </div>
          </div>

          <div v-if="canProbe" class="wiz-conn-actions wiz-check">
            <button
              type="button"
              class="btn ghost"
              :disabled="check.state === 'running'"
              @click="checkConnection"
            >
              {{ check.state === "running" ? "Checking…" : "Check connection" }}
            </button>
          </div>
          <!-- The verdict is a mark before the words — the Manage
               screen's own tick and "!", in its colours — so the eye
               gets the answer before reading what it was. -->
          <IssueNote
            v-if="check.state === 'failed' && check.failure"
            class="wiz-conn-note wiz-probe-failed"
            :failure="check.failure"
            :service="chosen.label"
            :where="signInWhere"
          />
          <p
            v-else-if="check.state === 'ok' && check.report"
            class="wiz-help wiz-conn-note wiz-probe-note wiz-probe-ok"
          >
            <svg class="wiz-probe-mark" viewBox="0 0 24 24" role="img" aria-label="Connected">
              <path :d="STATUS_GLYPHS.succeeded" fill="currentColor" />
            </svg>
            Connected to
            <b>{{ reachedName(check.report) }}</b
            ><!-- A message estimate is only shown when the provider gave
                  one for free: Gmail's profile carries it, JMAP's
                  session does not. --><template v-if="check.report.account.message_estimate">
              — about
              {{ check.report.account.message_estimate.toLocaleString() }} messages</template
            >.
            <!-- What the provider wanted said alongside a success. -->
            <span v-for="note in check.report.notes" :key="note" class="wiz-probe-aside">{{
              note
            }}</span>
          </p>
        </section>

        <label class="wiz-field">
          <span class="wiz-label">Name</span>
          <input v-model="name" class="wiz-input" />
          <small class="wiz-help">
            What this source is called on screen — anything you like, spaces and capitals included:
            <b>{{ nameHint }}</b
            >, say. Change it whenever you like: nothing on disk moves and no step re-runs. Leave it
            blank to be shown as <code>{{ groupId || "…" }}</code
            >.
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
            <b class="wiz-permanent">Permanent — this is your last chance to change it.</b>
            Suggested from the name. Creates
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
          search index has already recorded for every document in it, so it can’t change here. Use
          <b>Name</b> above for something you can.
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
                A second step, <code>{{ stepIdFor(groupId || "…", "render") }}</code
                >, turns what this brings in into markdown and makes it searchable. It runs on its
                own and can be re-run without fetching anything again. Turn it off and the data is
                still mirrored, but nothing about it reaches the grid or the search index.<template
                  v-if="renderWanted && section.fields.length === 0"
                >
                  It has no settings of its own.</template
                >
              </small>
            </label>
            <label class="wiz-field wiz-inline">
              <span class="wiz-label">Keyword-index the markdown</span>
              <input
                v-model="keywordWanted"
                type="checkbox"
                class="wiz-bool"
                :disabled="!renderWanted"
              />
              <small class="wiz-help">
                A step of its own, <code>{{ `${groupId || "…"}/keyword_index` }}</code
                >, puts this source's markdown into the index every free-text search goes to, so
                typing words into the search bar finds it. Turn it off and the source keeps its
                rows, its columns and its filters in the grid, but the search bar will not find
                it.<template v-if="!renderWanted">
                  Nothing to index while rendering is off.</template
                >
              </small>
            </label>
            <label class="wiz-field wiz-inline">
              <span class="wiz-label">Embed it for search by meaning</span>
              <input
                v-model="embedWanted"
                type="checkbox"
                class="wiz-bool"
                :disabled="!renderWanted || !keywordWanted"
              />
              <small class="wiz-help">
                Another step, <code>{{ `${groupId || "…"}/embed` }}</code
                >, computes vectors so a search matches on meaning as well as on words, and places
                the source on the map. Embedding is the slow part of a sync. Turn it off and keyword
                search still finds the source.<template v-if="renderWanted && !keywordWanted">
                  It reads the keyword index, so it needs that on.</template
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
            <span v-else-if="f.kind === 'bytes'" class="wiz-bytes">
              <input
                type="number"
                min="0"
                step="1"
                class="wiz-input wiz-num"
                :value="byteAmount(f)"
                @input="setByteAmount(f, ($event.target as HTMLInputElement).value)"
              />
              <select
                class="wiz-input wiz-select wiz-unit"
                :value="byteUnit(f)"
                @change="setByteUnit(f, ($event.target as HTMLSelectElement).value as ByteUnit)"
              >
                <option v-for="u in BYTE_UNITS" :key="u" :value="u">{{ u }}</option>
              </select>
            </span>
            <!-- Typed path + native picker. The input stays even in the
                 app: paste is a legitimate way in, and in a browser it is
                 the only one. docs/dev/wizard_file_pickers.md. -->
            <span v-else-if="f.kind === 'path'" class="wiz-pathrow">
              <input
                class="wiz-input wiz-path"
                :value="values[f.target] as string"
                spellcheck="false"
                @input="values[f.target] = ($event.target as HTMLInputElement).value"
              />
              <button v-if="canPick" type="button" class="btn ghost wiz-browse" @click="browse(f)">
                {{ f.picks === "file" ? "Choose file…" : "Choose folder…" }}
              </button>
            </span>
            <span v-else-if="f.kind === 'string_list'" class="wiz-listfield">
              <input
                class="wiz-input"
                :value="listText(f)"
                spellcheck="false"
                @input="setListText(f, ($event.target as HTMLInputElement).value)"
              />
              <!-- Loading a list is its own button, not part of the
                   check: a big account's list takes a while, so it is
                   asked for only where it is wanted, and says how far
                   it has got. -->
              <div v-if="f.probe && canProbe" class="wiz-load">
                <template v-if="listLoad(f.probe).state === 'running'">
                  <progress
                    class="wiz-load-bar"
                    :value="
                      listLoad(f.probe).progress?.total != null
                        ? listLoad(f.probe).progress?.done
                        : undefined
                    "
                    :max="listLoad(f.probe).progress?.total ?? undefined"
                  />
                  <small class="wiz-help wiz-load-status" role="status">{{
                    loadingLine(f.probe)
                  }}</small>
                </template>
                <button
                  v-else
                  type="button"
                  class="btn ghost wiz-load-btn"
                  @click="loadList(f.probe)"
                >
                  {{
                    listLoad(f.probe).state === "ok"
                      ? `Reload ${probeNoun(f.probe)}`
                      : `Load ${probeNoun(f.probe)} from ${chosen.label}`
                  }}
                </button>
                <small v-if="listLoad(f.probe).state === 'ok'" class="wiz-help wiz-load-done">
                  {{ loadedLine(f) }}
                  <span
                    v-for="note in listLoad(f.probe).report?.notes ?? []"
                    :key="note"
                    class="wiz-probe-aside"
                    >{{ note }}</span
                  >
                </small>
                <IssueNote
                  v-else-if="listLoad(f.probe).state === 'failed' && listLoad(f.probe).failure"
                  class="wiz-load-failed"
                  :failure="listLoad(f.probe).failure!"
                  :service="chosen.label"
                  :where="signInWhere"
                />
              </div>
              <!-- The picker is an *addition* to the box above, never
                   a replacement: a list needs credentials that may not
                   exist yet, and this form has to stay usable before one
                   has ever loaded. Both edit the same array. -->
              <ProbeItemPicker
                v-if="f.probe && probeOptions(f).length"
                :items="probeOptions(f)"
                :model-value="chosenValues(f)"
                @update:model-value="values[f.target] = $event"
              />
              <small v-if="f.probe && unknownValues(f).length" class="wiz-error">
                Not on this account: {{ unknownValues(f).join(", ") }}. Nothing can be mirrored for
                a name the account doesn’t have — check the spelling, or tick it in the list.
              </small>
            </span>
            <input
              v-else
              class="wiz-input"
              :value="values[f.target] as string"
              spellcheck="false"
              @input="values[f.target] = ($event.target as HTMLInputElement).value"
            />

            <small v-if="helpAroundStart(f)" class="wiz-help"
              >{{ helpAroundStart(f)!.before
              }}<span class="wiz-startin"
                ><code>{{ helpAroundStart(f)!.path }}</code
                ><button
                  type="button"
                  class="wiz-copy"
                  :title="copiedStart === f.target ? 'Copied' : 'Copy this path'"
                  :aria-label="`Copy ${helpAroundStart(f)!.path}`"
                  @click="copyStart(f)"
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path
                      :d="copiedStart === f.target ? STATUS_GLYPHS.succeeded : PATH_GLYPHS.copy"
                      fill="currentColor"
                    />
                  </svg></button></span
              >{{ helpAroundStart(f)!.after }}</small
            >
            <small v-else-if="f.help" class="wiz-help">{{ f.help }}</small>
            <small v-if="pickFailed[f.target]" class="wiz-error">
              Couldn’t open the file picker ({{ pickFailed[f.target] }}). Type or paste the path
              instead.
            </small>
          </label>
        </template>

        <label class="wiz-field">
          <span class="wiz-label">Description</span>
          <input v-model="description" class="wiz-input" />
          <small class="wiz-help">
            Optional. A sentence on what this source holds and what it is to you, for telling it
            apart from another of the same kind. Kept with the source's settings; nothing reads it
            yet. Change it whenever you like: nothing re-runs.
          </small>
        </label>

        <details class="wiz-review">
          <summary>Review the TOML this writes</summary>
          <pre>{{ preview }}</pre>
        </details>
      </div>

      <footer class="wiz-foot dialog-foot">
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

<style scoped src="./dialog.css"></style>
<style scoped>
.wiz {
  width: min(760px, 100%);
}

.wiz-filter,
.wiz-input {
  width: 100%;
  box-sizing: border-box;
  padding: 8px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
}
.wiz-filter {
  margin-bottom: 16px;
}
/* A bool field's own checkbox, sized as a box rather than stretched to
   the field's width like a text input. */
.wiz-bool {
  width: 16px;
  height: 16px;
}
/* Wide enough for the counts anyone types here, instead of stretching
   across the dialog the way a text field does. */
.wiz-num {
  width: 7em;
}
/* Amount and unit read as one control: the boxes touch, and only the
   outer corners are rounded. The focus ring belongs to the pair for the
   same reason — a ring around the amount alone is drawn along the seam
   and over the unit box beside it, splitting the one control back into
   two overlapping ones. */
.wiz-bytes {
  display: inline-flex;
  border-radius: var(--datalib-radius);
}
.wiz-bytes:focus-within {
  outline: 2px solid var(--datalib-accent);
  outline-offset: 1px;
}
.wiz-bytes .wiz-input:focus {
  outline: none;
}
.wiz-bytes .wiz-num {
  border-radius: var(--datalib-radius) 0 0 var(--datalib-radius);
}
.wiz-unit {
  width: auto;
  border-radius: 0 var(--datalib-radius) var(--datalib-radius) 0;
  border-left: none;
}
/* Shares `.wiz-input`'s box; keeps the platform disclosure arrow so it
   doesn't read as a text field you can type into. */
.wiz-select {
  cursor: pointer;
}

.wiz-group h3 {
  font-size: var(--datalib-font-size-small);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--datalib-muted);
  margin: 16px 0 8px;
}
.wiz-tiles {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 8px;
}
.wiz-tile {
  display: flex;
  align-items: center;
  gap: 10px;
  text-align: left;
  padding: 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  color: inherit;
  cursor: pointer;
  font: inherit;
}
.wiz-tile:hover:not(:disabled) {
  background: var(--datalib-hover);
}
.wiz-tile.cursor {
  outline: 2px solid var(--datalib-accent);
  outline-offset: -1px;
}
.wiz-tile.soon {
  opacity: 0.55;
  cursor: not-allowed;
}
.wiz-tile-text {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
}
.wiz-tile-text b {
  font-size: var(--datalib-title-size);
}
.wiz-tile-text small {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.wiz-soon {
  font-size: 10px;
  color: var(--datalib-muted);
  border: 1px solid var(--datalib-border);
  border-radius: 3px;
  padding: 1px 4px;
  white-space: nowrap;
}
.wiz-icon {
  width: 22px;
  height: 22px;
  flex: none;
}
.wiz-icon-fallback {
  color: var(--datalib-muted);
  font-size: 18px;
  text-align: center;
}

.wiz-chosen {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface-2);
  margin-bottom: 14px;
}
.wiz-chosen div {
  flex: 1;
  display: flex;
  flex-direction: column;
}
.wiz-chosen small {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}

.wiz-cred {
  font-size: var(--datalib-font-size);
  color: var(--datalib-muted);
  border-left: 3px solid var(--datalib-border);
  padding-left: 10px;
  margin: 0 0 16px;
}

.wiz-field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-bottom: 16px;
}
/* Label and control on one line, with the help text wrapping to its own
   full-width row beneath them. */
.wiz-field.wiz-inline {
  flex-direction: row;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 8px;
}
.wiz-field.wiz-inline .wiz-help,
.wiz-field.wiz-inline .wiz-error {
  flex: 1 0 100%;
}
/* A tickbox reads as "[x] thing", not "thing [x]". */
.wiz-field.wiz-inline .wiz-bool {
  order: -1;
}
.wiz-label {
  font-size: var(--datalib-font-size);
  font-weight: 600;
}
.wiz-nofields {
  margin: 0 0 16px;
}
/* The id where it is a fact rather than a field, and the id error that
   then has nowhere else to go. Both sit in the form's flow. */
.wiz-fixed-id {
  margin: 0 0 16px;
}
.wiz-help {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
  line-height: 1.45;
}
.wiz-error {
  color: var(--datalib-error-fg);
  font-size: var(--datalib-font-size-small);
}
.wiz-permanent {
  color: var(--datalib-error-fg);
}
.wiz-probe-mark {
  width: 14px;
  height: 14px;
  vertical-align: -3px;
  margin-right: 3px;
}
.wiz-probe-ok .wiz-probe-mark {
  color: var(--datalib-log-ok);
}
.wiz-probe-aside {
  display: block;
  margin-top: 2px;
}
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
  border-radius: var(--datalib-radius);
  font-size: var(--datalib-font-size-small);
  line-height: 1.5;
}
.wiz-probe-note details > summary {
  cursor: pointer;
}
/* A picker's own "Load" row: the button, or while it runs a bar and a
   line that counts. The bar has no value until the service says a
   total, which leaves it in the browser's moving, indeterminate state. */
.wiz-load {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 10px;
  margin: 6px 0;
}
.wiz-load-bar {
  width: 160px;
  height: 6px;
  accent-color: var(--datalib-accent);
}
.wiz-load-failed {
  flex-basis: 100%;
}
.wiz-convert {
  border-left: 3px solid var(--datalib-border);
  padding-left: 10px;
}
.wiz-convert-head {
  margin: 0;
}
.wiz-signin {
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  margin-bottom: 10px;
}
.wiz-tabs {
  display: flex;
  border-bottom: 1px solid var(--datalib-border);
}
.wiz-tab {
  padding: 8px 14px;
  border: 0;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
  background: none;
  color: var(--datalib-muted);
  font: inherit;
  cursor: pointer;
}
.wiz-tab[aria-selected="true"] {
  color: var(--datalib-fg);
  border-bottom-color: var(--datalib-accent);
}
.wiz-tabpanel {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px;
}
.wiz-tabpanel > p,
.wiz-tabpanel > .wiz-conn-actions {
  margin: 0;
}
.wiz-paste p,
.wiz-paste .wiz-field {
  margin: 0;
}
.wiz-req {
  font-style: normal;
  font-weight: 400;
  font-size: 10.5px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--datalib-muted);
  margin-left: 6px;
}
.wiz-path {
  font-family: var(--datalib-mono);
  font-size: var(--datalib-font-size);
}
/* The input takes the slack so the button keeps its label on one line. */
.wiz-pathrow {
  display: flex;
  gap: 8px;
  align-items: center;
}
.wiz-pathrow .wiz-input {
  flex: 1;
  min-width: 0;
}
.wiz-browse {
  white-space: nowrap;
}
.wiz-startin {
  white-space: nowrap;
}
.wiz-startin code {
  font-size: 11px;
  user-select: all;
}
.wiz-copy {
  display: inline-flex;
  vertical-align: -3px;
  margin-left: 2px;
  padding: 1px;
  border: none;
  background: none;
  color: inherit;
  cursor: pointer;
}
.wiz-copy:hover {
  color: var(--datalib-fg);
}
.wiz-copy svg {
  width: 13px;
  height: 13px;
}
.wiz-foot-note {
  margin-right: auto;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-muted);
}

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
  font-size: var(--datalib-font-size-small);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--datalib-muted);
}
/* The section's own toggle sits flush under its heading. */
.wiz-section > .wiz-field {
  margin-bottom: 0;
}

/* The Connection block: latchkey account + the two buttons. Boxed
   because it is about the *account*, not about one setting — the
   fields below it are all things you type, and this is the one place
   that talks to something outside. */
.wiz-conn {
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  padding: 12px 14px 4px;
  margin-bottom: 16px;
}
.wiz-conn-intro {
  margin: 0 0 12px;
}
.wiz-conn-actions {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
  margin-bottom: 10px;
}
.wiz-conn-note {
  margin: 0 0 10px;
}
.wiz-listfield {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.wiz-review {
  margin-top: 8px;
}
.wiz-review summary {
  cursor: pointer;
  font-size: var(--datalib-font-size);
  color: var(--datalib-muted);
}
.wiz-review pre {
  margin: 8px 0 0;
  padding: 10px;
  background: var(--datalib-code-bg);
  border-radius: var(--datalib-radius);
  overflow-x: auto;
  font-size: var(--datalib-font-size-small);
}
</style>
