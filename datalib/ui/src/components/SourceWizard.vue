<script setup lang="ts">
// The "Add Data Source" / "Edit" flow: pick a type, fill its form,
// review the TOML that will be written.
//
// One component serves both verbs — the design's point is that create
// and edit are the same descriptor driven two ways (docs/dev/
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
// pair rather than surgically editing values — which is why the caller
// only offers Edit when `paramsAreRepresentable` said yes.
import { computed, ref, watch } from "vue";
import {
  CATALOG,
  KIND_LABELS,
  filterCatalog,
  type CatalogEntry,
  type Field,
} from "@/config/catalog";
import {
  buildStepPair,
  getParam,
  slugify,
  suggestId,
  type ConfiguredSource,
  type FieldValues,
} from "@/config/sourceSteps";
import { iconUrl } from "@/config/icons";
import { isDesktopApp, pickPath } from "@/desktop";

const props = defineProps<{
  /// Ids already in the config, so a new source can't collide.
  takenIds: Set<string>;
  /// Present → edit that source instead of creating one.
  editing?: { source: ConfiguredSource; entry: CatalogEntry } | null;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (e: "submit", payload: { id: string; name: string; body: string; entry: CatalogEntry }): void;
}>();

type Stage = "pick" | "configure";

const stage = ref<Stage>(props.editing ? "configure" : "pick");
const query = ref("");
const chosen = ref<CatalogEntry | null>(props.editing?.entry ?? null);
/// Blank means "no name" — `listConfiguredSources` reports the id in
/// that case, so the field shows the id as its placeholder rather than
/// pre-filling one, and clearing it removes the key.
const name = ref(
  props.editing && props.editing.source.name !== props.editing.source.id
    ? props.editing.source.name
    : "",
);
const id = ref(props.editing?.source.id ?? "");
const values = ref<FieldValues>({});
/// Once the id has been typed into directly, the name stops driving it.
/// A derived id is a convenience, never something that overwrites a
/// choice the user made.
const idTouched = ref(false);

const isEdit = computed(() => !!props.editing);

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

function seedValues(entry: CatalogEntry, source?: ConfiguredSource) {
  const next: FieldValues = {};
  for (const field of entry.fields ?? []) {
    const existing = source
      ? source.steps
          .map((s) => getParam(s.params, field.target))
          .find((v) => v !== undefined)
      : undefined;
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

if (props.editing) seedValues(props.editing.entry, props.editing.source);

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
  if (isEdit.value || idTouched.value || !chosen.value) return;
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

/// An id becomes a directory component and a step-id stem, so it has to
/// be a portable path segment. Mirrors the rules `migrate_config`'s
/// `validate_source_name` applies on the YAML path; the reserved ids
/// match `dag::config::RESERVED_STANZA_NAMES`.
const RESERVED = new Set(["system", "unified_index"]);
const idError = computed(() => {
  const n = id.value.trim();
  if (!n) return "An id is required.";
  if (RESERVED.has(n)) return `"${n}" is reserved — it names a directory the pipeline owns.`;
  if (n === "." || n === "..") return "The id must not be '.' or '..'.";
  if (n.startsWith("-")) return "The id must not start with '-'.";
  if (!/^[A-Za-z0-9._-]+$/.test(n))
    return "Use only letters, digits, '.', '_' and '-' — the id becomes a directory.";
  if (!isEdit.value && props.takenIds.has(n)) return `"${n}" is already configured.`;
  return null;
});

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

const body = computed(() =>
  chosen.value
    ? buildStepPair(chosen.value, id.value.trim(), name.value, values.value)
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
  emit("submit", {
    id: id.value.trim(),
    name: name.value.trim(),
    body: body.value,
    entry: chosen.value,
  });
}
</script>

<template>
  <div class="wiz-backdrop" @click.self="emit('close')">
    <div class="wiz" role="dialog" aria-modal="true" :aria-label="isEdit ? 'Edit data source' : 'Add data source'">
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
          <button v-if="!isEdit" class="btn ghost" @click="stage = 'pick'">Change</button>
        </div>

        <p v-if="chosen.credentialService" class="wiz-cred">
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
            :disabled="isEdit"
            spellcheck="false"
            @input="idTouched = true"
          />
          <small v-if="isEdit" class="wiz-help">
            Fixed: the id is this source’s folder on disk and the path the search index has already
            recorded for every document in it, so changing it is a migration rather than an edit.
            Use Name above for something you can change.
          </small>
          <small v-else class="wiz-help">
            Suggested from the name, and yours to override. Creates
            <code>{{ id || "…" }}/raw</code>
            <template v-if="chosen?.renderStep !== false">
              and <code>{{ id || "…" }}/rendered_md</code>
            </template>
            under the data root, and the stem of the step ids.
          </small>
          <small v-if="idError && (idTouched || isEdit)" class="wiz-error">{{ idError }}</small>
        </label>

        <label v-for="f in chosen.fields ?? []" :key="f.target" class="wiz-field">
          <span class="wiz-label">
            {{ f.label }}
            <em v-if="'required' in f && f.required" class="wiz-req">required</em>
          </span>

          <input
            v-if="f.kind === 'bool'"
            type="checkbox"
            class="wiz-check"
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

        <details class="wiz-review">
          <summary>Review the TOML this writes</summary>
          <pre>{{ body }}</pre>
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
.wiz-check { width: 16px; height: 16px; }

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
