<script setup lang="ts">
// The "Add Data Source" / "Edit" flow: pick a type, fill its form,
// review the TOML that will be written.
//
// One component serves both verbs — the design's point is that create
// and edit are the same descriptor driven two ways (docs/dev/
// source_wizard.md). In edit mode the type is fixed and the name is
// read-only: renaming a source would move its directory on disk and
// orphan its raw store, which is a migration, not a form field.
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
  suggestName,
  type ConfiguredSource,
  type FieldValues,
} from "@/config/sourceSteps";
import { iconUrl } from "@/config/icons";

const props = defineProps<{
  /// Names already in the config, so a new source can't collide.
  takenNames: Set<string>;
  /// Present → edit that source instead of creating one.
  editing?: { source: ConfiguredSource; entry: CatalogEntry } | null;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (e: "submit", payload: { name: string; body: string; entry: CatalogEntry }): void;
}>();

type Stage = "pick" | "configure";

const stage = ref<Stage>(props.editing ? "configure" : "pick");
const query = ref("");
const chosen = ref<CatalogEntry | null>(props.editing?.entry ?? null);
const name = ref(props.editing?.source.name ?? "");
const values = ref<FieldValues>({});
const nameTouched = ref(false);

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
  name.value = suggestName(props.takenNames, entry.defaultName);
  nameTouched.value = false;
  seedValues(entry);
  stage.value = "configure";
}

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

/// A source name becomes a directory component and a step-id stem, so
/// it has to be a portable path segment. Mirrors the rules
/// `migrate_config`'s `validate_source_name` applies on the YAML path;
/// the reserved names match `dag::config::RESERVED_STANZA_NAMES`.
const RESERVED = new Set(["system", "unified_index"]);
const nameError = computed(() => {
  const n = name.value.trim();
  if (!n) return "A name is required.";
  if (RESERVED.has(n)) return `"${n}" is reserved — it names a directory the pipeline owns.`;
  if (n === "." || n === "..") return "Name must not be '.' or '..'.";
  if (n.startsWith("-")) return "Name must not start with '-'.";
  if (!/^[A-Za-z0-9._-]+$/.test(n))
    return "Use only letters, digits, '.', '_' and '-' — the name becomes a directory.";
  if (!isEdit.value && props.takenNames.has(n)) return `"${n}" is already configured.`;
  return null;
});

const body = computed(() =>
  chosen.value ? buildStepPair(chosen.value, name.value.trim(), values.value) : "",
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

function submit() {
  if (nameError.value || !chosen.value) return;
  emit("submit", { name: name.value.trim(), body: body.value, entry: chosen.value });
}
</script>

<template>
  <div class="wiz-backdrop" @click.self="emit('close')">
    <div class="wiz" role="dialog" aria-modal="true" :aria-label="isEdit ? 'Edit data source' : 'Add data source'">
      <header class="wiz-head">
        <h2>{{ isEdit ? `Edit ${name}` : "Add a data source" }}</h2>
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
            :disabled="isEdit"
            spellcheck="false"
            @input="nameTouched = true"
          />
          <small v-if="isEdit" class="wiz-help">
            Renaming would move the source’s directory and orphan its raw store, so it’s fixed here.
          </small>
          <small v-else class="wiz-help">
            Becomes <code>{{ name || "…" }}/raw</code> under the data root, and the stem of both step ids.
          </small>
          <small v-if="nameError && (nameTouched || isEdit)" class="wiz-error">{{ nameError }}</small>
        </label>

        <label v-for="f in chosen.fields ?? []" :key="f.target" class="wiz-field">
          <span class="wiz-label">{{ f.label }}</span>

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
        </label>

        <details class="wiz-review">
          <summary>Review the TOML this writes</summary>
          <pre>{{ body }}</pre>
        </details>
      </div>

      <footer class="wiz-foot">
        <button class="btn ghost" @click="emit('close')">Cancel</button>
        <button
          v-if="stage === 'configure'"
          class="btn primary"
          :disabled="!!nameError"
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
