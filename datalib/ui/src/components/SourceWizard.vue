<script setup lang="ts">
// The "Add Data Source" / "Edit" flow: pick a type, fill its form,
// review the TOML that will be written.
//
// **It configures one step.** A fetch step and the render step that
// reads it are two separate rows, two separate forms, and two separate
// things to run — there is no "source" object here.
//
// Adding the render step is offered at the end of a fetch step's flow,
// and *how* depends on whether there is anything to ask. Only
// `signal_backup` declares a render-phase field today, so for every
// other provider the render step has no configuration at all: it gets a
// checkbox here rather than a second dialog, because a dialog with
// nothing in it is a dialog that shouldn't exist. When the provider
// does have render knobs, this same component opens again for the
// render step, pre-filled and pointed at the step just written.
//
// One component serves create and edit — the design's point is that
// they are the same descriptor driven two ways (docs/dev/
// source_wizard.md).
//
// Two fields carry the identity, and only one of them is permanent.
// **Name** is what you type and what every screen shows; it is free
// text and always editable. **Id** is the directory on disk and the
// prefix inside every `qmd_path` the index holds, so changing it is a
// migration rather than an edit — it is derived from the name once, at
// creation, and read-only forever after.
//

// What this does NOT do yet, and the design says it eventually must:
// no credential screen (needs the latchkey endpoints), no live channel
// picker (needs `datalib-step probe`), and edit regenerates the step
// rather than surgically editing values — which is why the caller only
// offers Edit when `paramsAreRepresentable` said yes.
import { computed, ref, watch } from "vue";
import {
  CATALOG,
  KIND_LABELS,
  filterCatalog,
  type CatalogEntry,
  type Field,
} from "@/config/catalog";
import {
  buildStep,
  fieldIsActive,
  fieldPhaseOf,
  fieldsFor,
  getParam,
  renderIdFor,
  slugify,
  suggestId,
  type ConfiguredStep,
  type FieldValues,
} from "@/config/sourceSteps";
import type { FieldPhase } from "@/config/catalog";
import { iconUrl } from "@/config/icons";
import { isDesktopApp, pickPath } from "@/desktop";

const props = defineProps<{
  /// Id stems already in the config, so a new step can't collide with
  /// a tree that exists. A stem rather than a full id: creating
  /// `work-slack/raw` reserves `work-slack/` for its render sibling
  /// too.
  takenIds: Set<string>;
  /// Present → edit that step instead of creating one.
  editing?: { step: ConfiguredStep; entry: CatalogEntry } | null;
  /// Present → create the render step that reads this fetch step,
  /// pre-filled from it. The chained half of "also render this?".
  renderFor?: { fetchId: string; fetchName: string; entry: CatalogEntry } | null;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (
    e: "submit",
    payload: {
      id: string;
      name: string;
      body: string;
      entry: CatalogEntry;
      phase: FieldPhase;
      /// Set when this step reads another — the render step's fetch
      /// step. The caller wires fan-ins off it.
      inputs: string[];
      /// The fetch step just written, when the caller should now open a
      /// second dialog to render it — only for providers whose render
      /// step has options. Null otherwise.
      offerRenderFor: { fetchId: string; fetchName: string } | null;
      /// The render step to write alongside this one, when the user
      /// left the checkbox ticked and there was nothing to ask about.
      /// Null when a render step is not being created here.
      alsoRender: { id: string; body: string } | null;
    },
  ): void;
}>();

type Stage = "pick" | "configure";

/// Which of the three ways this dialog was opened. Only `create` shows
/// the type picker: editing knows its type, and a chained render step
/// inherits its fetch step's.
const mode = computed<"create" | "edit" | "render">(() =>
  props.editing ? "edit" : props.renderFor ? "render" : "create",
);
const isEdit = computed(() => mode.value === "edit");

/// Which half of the descriptor's fields this dialog writes, and which
/// `datalib-step` subcommand the step gets.
const phase = computed<FieldPhase>(() => {
  if (props.editing) return fieldPhaseOf(props.editing.step);
  return props.renderFor ? "render" : "download";
});

const stage = ref<Stage>(props.editing || props.renderFor ? "configure" : "pick");
const query = ref("");
const chosen = ref<CatalogEntry | null>(
  props.editing?.entry ?? props.renderFor?.entry ?? null,
);

/// Blank means "no name" — a step with none is shown by its id, so the
/// field takes the id as its placeholder rather than pre-filling one,
/// and clearing it removes the key.
const name = ref(
  props.editing && props.editing.step.name !== props.editing.step.id
    ? props.editing.step.name
    : props.renderFor
      ? `${props.renderFor.fetchName} (render markdown)`
      : "",
);
const id = ref(
  props.editing?.step.id ?? (props.renderFor ? renderIdFor(props.renderFor.fetchId) : ""),
);
const values = ref<FieldValues>({});
/// Once the id has been typed into directly, the name stops driving it.
/// A derived id is a convenience, never something that overwrites a
/// choice the user made. A chained render step starts touched: its id
/// is the sibling of a step that already exists, not a guess from a
/// name.
const idTouched = ref(!!props.renderFor);

/// The fields this dialog shows. Empty for most render steps, which is
/// why one is usually a checkbox below rather than a dialog of its own.
///
/// Also drops fields whose `requires` gate is shut. `buildStep` applies
/// the same gate when it writes the TOML, and the two have to agree —
/// otherwise the review pane shows a setting the form isn't offering.
const shownFields = computed(() =>
  chosen.value
    ? fieldsFor(chosen.value, phase.value).filter((f) => fieldIsActive(f, values.value))
    : [],
);

/// Does this provider's render step have anything to configure? The
/// question that decides checkbox-or-dialog.
const renderHasOptions = computed(
  () => !!chosen.value && fieldsFor(chosen.value, "render").length > 0,
);

/// Can a render step be offered at all here — a new fetch step, for a
/// provider that produces markdown?
const canOfferRender = computed(
  () =>
    mode.value === "create" &&
    phase.value === "download" &&
    !!chosen.value &&
    chosen.value.renderStep !== false,
);

/// Ticked by default: rendering is what makes the data searchable, and
/// a fetch step on its own is the unusual choice. Only shown when the
/// render step has nothing to ask about.
const alsoRender = ref(true);

/// The render step's TOML, for the review pane, so the checkbox shows
/// its consequence rather than asserting it.
const alsoRenderPreview = computed(() => {
  if (!chosen.value || !canOfferRender.value || renderHasOptions.value || !alsoRender.value) {
    return "";
  }
  const fetchName = name.value.trim() || stepId.value;
  return `\n\n${buildStep({
    entry: chosen.value,
    id: renderIdFor(stepId.value),
    name: `${fetchName} (render markdown)`,
    phase: "render",
    inputs: [stepId.value],
    values: values.value,
  })}`;
});

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

function seedValues(entry: CatalogEntry, step?: ConfiguredStep) {
  const next: FieldValues = {};
  for (const field of entry.fields ?? []) {
    const existing = step ? getParam(step.params, field.target) : undefined;
    if (existing !== undefined) {
      next[field.target] =
        field.kind === "string_list" ? (existing as string[]) ?? [] : existing;
    } else if (field.kind === "bool") {
      next[field.target] = field.default ?? false;
    } else if (field.kind === "string_list") {
      next[field.target] = [];
    } else {
      next[field.target] = "";
    }
  }
  values.value = next;
}

if (props.editing) seedValues(props.editing.entry, props.editing.step);
else if (props.renderFor) seedValues(props.renderFor.entry);

function choose(entry: CatalogEntry) {
  if (!entry.wizard) return;
  chosen.value = entry;
  // No name typed yet, so the id starts from the catalog's default.
  // Typing a name re-derives it, until the id is touched directly.
  id.value = suggestId(props.takenIds, "", entry.defaultName);
  idTouched.value = false;
  seedValues(entry);
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

/// The id the *user* edits is the stem: creating a fetch step means
/// choosing `work-slack`, and the steps written under it are
/// `work-slack/raw` and (later) `work-slack/rendered_md`. So the field
/// validates a single path segment, and the step ids are built from it.
///
/// Mirrors the rules `migrate_config`'s `validate_source_name` applies
/// on the YAML path, and the segment rules `dag::config` enforces on a
/// step id. `system` is reserved by the loader; `unified_index` is not
/// any more (the index steps own it by *being* it) but proposing it
/// would collide, so the wizard still declines to.
const RESERVED = new Set(["system", "unified_index"]);
const idError = computed(() => {
  const n = stem.value;
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

/// The stem the user typed. In edit and render mode the id field holds
/// a full step id (`work-slack/raw`), so the stem is its first segment;
/// while creating, the field *is* the stem.
const stem = computed(() => {
  const raw = id.value.trim();
  return mode.value === "create" ? raw : raw.split("/")[0];
});

/// The step id actually written: `<stem>/raw` or `<stem>/rendered_md`
/// while creating, and the id as-is when editing or chaining (both of
/// which start from a real step id).
const stepId = computed(() =>
  mode.value === "create"
    ? `${stem.value}/${phase.value === "render" ? "rendered_md" : "raw"}`
    : id.value.trim(),
);

/// Fields the provider's Rust struct declares non-optional — a
/// `PathBuf` rather than an `Option<PathBuf>` — so a config missing one
/// fails at deserialize time rather than at sync time. Caught here so
/// the message lands under the field instead of in a job log.
const missingRequired = computed(() =>
  (chosen.value?.fields ?? [])
    .filter((f) => "required" in f && f.required)
    .filter((f) => String(values.value[f.target] ?? "").trim() === "")
    .map((f) => f.label),
);

const canSubmit = computed(() => !idError.value && missingRequired.value.length === 0);

/// What this step reads. A fetch step names nothing — its real input is
/// a remote service, or a path in its own params. A render step names
/// the fetch step it was chained from, or (when editing) whatever it
/// already declared.
const inputs = computed<string[]>(() => {
  if (props.renderFor) return [props.renderFor.fetchId];
  if (props.editing) return props.editing.step.inputs;
  return [];
});

const body = computed(() =>
  chosen.value
    ? buildStep({
        entry: chosen.value,
        id: stepId.value,
        name: name.value,
        phase: phase.value,
        inputs: inputs.value,
        values: values.value,
      })
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

function submit() {
  if (!canSubmit.value || !chosen.value) return;
  const fetchName = name.value.trim() || stepId.value;

  // The render step comes one of two ways, never both. With options, a
  // second dialog the caller opens; without, the checkbox above, and
  // the step is written here alongside the fetch step.
  const offer =
    canOfferRender.value && renderHasOptions.value
      ? { fetchId: stepId.value, fetchName }
      : null;
  const alsoBody =
    canOfferRender.value && !renderHasOptions.value && alsoRender.value
      ? {
          id: renderIdFor(stepId.value),
          body: buildStep({
            entry: chosen.value,
            id: renderIdFor(stepId.value),
            name: `${fetchName} (render markdown)`,
            phase: "render",
            inputs: [stepId.value],
            values: values.value,
          }),
        }
      : null;

  emit("submit", {
    id: stepId.value,
    name: name.value.trim(),
    body: body.value,
    entry: chosen.value,
    phase: phase.value,
    inputs: inputs.value,
    offerRenderFor: offer,
    alsoRender: alsoBody,
  });
}
</script>

<template>
  <div class="wiz-backdrop" @click.self="emit('close')">
    <div class="wiz" role="dialog" aria-modal="true" :aria-label="mode === 'edit' ? 'Edit step' : mode === 'render' ? 'Add render step' : 'Add data source'">
      <header class="wiz-head">
        <h2>
          {{
            mode === "edit"
              ? `Edit ${name || id}`
              : mode === "render"
                ? "Render to markdown"
                : "Add a data source"
          }}
        </h2>
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
              :key="e.type"
              class="wiz-tile"
              :class="{ soon: !e.wizard, cursor: flat[cursor]?.type === e.type }"
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

        <p v-if="mode === 'render'" class="wiz-cred">
          A second step that turns what
          <code>{{ renderFor?.fetchId }}</code> downloaded into markdown, and makes it
          searchable. It runs on its own and can be re-run without re-downloading anything.
        </p>

        <p v-if="chosen.credentialService && phase === 'download'" class="wiz-cred">
          Credentials come from latchkey’s <code>{{ chosen.credentialService }}</code> service.
          Connecting from here isn’t wired up yet — if a sync fails on auth, the job log carries
          the exact command to run.
        </p>

        <label class="wiz-field">
          <span class="wiz-label">Name</span>
          <input
            v-model="name"
            class="wiz-input"
            :placeholder="id || '…'"
          />
          <small class="wiz-help">
            What this is called on screen. Change it whenever you like — nothing on disk moves and
            no step re-runs. Leave it blank to be shown as <code>{{ id || "…" }}</code>.
          </small>
        </label>

        <label class="wiz-field">
          <span class="wiz-label">Id</span>
          <input
            v-model="id"
            class="wiz-input"
            :disabled="mode !== 'create'"
            spellcheck="false"
            @input="idTouched = true"
          />
          <small v-if="isEdit" class="wiz-help">
            Fixed: the id is this step’s folder on disk and the path the search index has already
            recorded for every document in it, so changing it is a migration rather than an edit.
            Use Name above for something you can change.
          </small>
          <small v-else-if="mode === 'render'" class="wiz-help">
            Fixed: the sibling of the step it reads, so both live under
            <code>{{ id.split("/")[0] }}/</code> on disk.
          </small>
          <small v-else class="wiz-help">
            Suggested from the name, and yours to override. Creates
            <code>{{ stepId }}</code> under the data root.
            <template v-if="chosen?.renderStep !== false">
              A render step, if you add one, becomes
              <code>{{ stem || "…" }}/rendered_md</code> beside it.
            </template>
          </small>
          <small v-if="idError && (idTouched || mode !== 'create')" class="wiz-error">
            {{ idError }}
          </small>
        </label>

        <p v-if="shownFields.length === 0 && mode !== 'create'" class="wiz-help wiz-nofields">
          This step has no options — its id, its name and what it reads are its whole
          configuration.
        </p>

        <label v-for="f in shownFields" :key="f.target" class="wiz-field">
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
            class="wiz-input"
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
          <input
            v-else-if="f.kind === 'string_list'"
            class="wiz-input"
            :placeholder="f.placeholder"
            :value="listText(f)"
            spellcheck="false"
            @input="setListText(f, ($event.target as HTMLInputElement).value)"
          />
          <input
            v-else
            class="wiz-input"
            :placeholder="f.placeholder"
            :value="values[f.target] as string"
            @input="values[f.target] = ($event.target as HTMLInputElement).value"
          />

          <small v-if="f.help" class="wiz-help">{{ f.help }}</small>
          <small v-if="pickFailed[f.target]" class="wiz-error">
            Couldn’t open the file picker ({{ pickFailed[f.target] }}). Type or paste the path
            instead.
          </small>
        </label>

        <label v-if="canOfferRender && !renderHasOptions" class="wiz-check">
          <input v-model="alsoRender" type="checkbox" />
          <span>
            <b>Also render this to markdown</b>
            <small>
              A second step, <code>{{ stem || "…" }}/rendered_md</code>, that turns what this
              downloads into markdown and makes it searchable. It has no settings of its own,
              runs separately, and can be added later from the row’s actions.
            </small>
          </span>
        </label>

        <details class="wiz-review">
          <summary>Review the TOML this writes</summary>
          <pre>{{ body }}{{ alsoRenderPreview }}</pre>
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
          {{ mode === "edit" ? "Save changes" : mode === "render" ? "Add render step" : "Add source" }}
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
/* A bool field's own checkbox. Named apart from `.wiz-check` — the
   "also render this" card further down — because the two were one
   class, and this 16px box was being applied to that whole card:
   its label and its paragraph of help got a 16px-wide column to wrap
   inside, and overlapped the disclosure below it. Two elements, two
   names. */
.wiz-bool { width: 16px; height: 16px; }

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
.wiz-label { font-size: 12.5px; font-weight: 600; }
.wiz-nofields { margin: 0; }
.wiz-check {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  padding: 10px 12px;
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
}
.wiz-check input { margin-top: 3px; }
.wiz-check span { display: flex; flex-direction: column; gap: 3px; }
.wiz-check small { color: var(--datalib-muted); }
.wiz-help { color: var(--datalib-muted); font-size: 11.5px; line-height: 1.45; }
.wiz-error { color: #b8481a; font-size: 11.5px; }
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
