<script setup lang="ts">
// "Compare two syncs…" on a source: pick two commits of its raw store
// and a name, and a diff group is written between them —
// docs/dev/plans/completed/diff_renderer.md. The commits come from the source's
// ingest tree's history; the newest is the default `to` and the one
// before it the default `from`, which is "what the last sync changed".
import { computed, onMounted, ref } from "vue";

import { fetchTreeHistory, type HistoryCommit } from "@/api";
import { slugify, suggestId } from "@/config/sourceSteps";

const props = defineProps<{
  source: { id: string; name: string };
  /// Group ids already in the config, so the new one lands on a free tree.
  takenIds: Set<string>;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  (
    e: "submit",
    payload: {
      id: string;
      name: string;
      source: string;
      from: string;
      to: string;
      maxDocuments: number;
    },
  ): void;
}>();

const DEFAULT_MAX_DOCUMENTS = 1000;

const commits = ref<HistoryCommit[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const from = ref("");
const to = ref("");
const name = ref(`${props.source.name} · changes`);
const maxDocuments = ref(DEFAULT_MAX_DOCUMENTS);

const id = computed(() =>
  suggestId(props.takenIds, slugify(name.value), `${props.source.id}-diff`),
);

/// The commits of the store that holds the source's records — the
/// `entities` store when the tree has several — newest first.
function recordCommits(stores: { path: string; commits: HistoryCommit[] }[]): HistoryCommit[] {
  const entities = stores.find((s) => s.path.endsWith("entities.doltlite_db"));
  return (entities ?? stores[0])?.commits ?? [];
}

onMounted(async () => {
  try {
    const history = await fetchTreeHistory(`${props.source.id}/ingest`);
    commits.value = recordCommits(history.stores);
    if (commits.value.length < 2) {
      error.value =
        commits.value.length === 0
          ? "This source has no synced data yet."
          : "This source has synced once: there is nothing earlier to compare it with.";
    } else {
      to.value = commits.value[0].hash;
      from.value = commits.value[1].hash;
    }
  } catch (e) {
    const message = (e as Error).message;
    // The history route answers 404 for a tree nothing writes: a source
    // with no ingest step has no raw store, and so no syncs to compare.
    error.value = /\b404\b/.test(message)
      ? "This source has no ingest step, so there are no syncs to compare."
      : message;
  } finally {
    loading.value = false;
  }
});

const ready = computed(
  () =>
    !loading.value &&
    !error.value &&
    from.value !== "" &&
    to.value !== "" &&
    from.value !== to.value &&
    name.value.trim() !== "" &&
    maxDocuments.value >= 1,
);

/// How a commit reads in the pickers: when, and what the sync said.
function label(c: HistoryCommit): string {
  const when = c.date.replace("T", " ").replace(/\.\d+/, "").replace(/[+-]00:00$|Z$/, " UTC");
  return `${when} — ${c.message} (${c.hash.slice(0, 8)})`;
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") emit("close");
}

function submit() {
  if (!ready.value) return;
  emit("submit", {
    id: id.value,
    name: name.value.trim(),
    source: props.source.id,
    from: from.value,
    to: to.value,
    maxDocuments: Math.floor(maxDocuments.value),
  });
}
</script>

<template>
  <div class="cmp-backdrop" @click.self="emit('close')" @keydown="onKeydown">
    <div class="cmp" role="dialog" aria-modal="true" aria-label="Compare two syncs">
      <header class="cmp-head">
        <h2>Compare two syncs of {{ source.name }}</h2>
        <button class="cmp-x" aria-label="Close" @click="emit('close')">×</button>
      </header>
      <div class="cmp-body">
        <p class="cmp-blurb">
          A comparison is a source of its own: every record that was added, removed or
          changed between two syncs, as documents with the changes marked and rows the grid
          colours. It stays as it is until you compare again or remove it.
        </p>
        <p v-if="loading" class="cmp-note">Reading the sync history…</p>
        <p v-else-if="error" class="cmp-error">{{ error }}</p>
        <template v-else>
          <label class="cmp-field">
            <span class="cmp-label">From</span>
            <select v-model="from" class="cmp-input">
              <option v-for="c in commits" :key="c.hash" :value="c.hash">{{ label(c) }}</option>
            </select>
          </label>
          <label class="cmp-field">
            <span class="cmp-label">To</span>
            <select v-model="to" class="cmp-input">
              <option v-for="c in commits" :key="c.hash" :value="c.hash">{{ label(c) }}</option>
            </select>
          </label>
          <p v-if="from === to" class="cmp-error">Pick two different syncs.</p>
          <label class="cmp-field">
            <span class="cmp-label">Name</span>
            <input v-model="name" class="cmp-input" type="text" />
            <span class="cmp-hint">id: <code>{{ id }}</code></span>
          </label>
          <label class="cmp-field">
            <span class="cmp-label">At most this many documents</span>
            <input v-model.number="maxDocuments" class="cmp-input cmp-num" type="number" min="1" />
            <span class="cmp-hint">
              A comparison larger than this fails rather than rendering everything.
            </span>
          </label>
        </template>
      </div>
      <footer class="cmp-foot">
        <button class="btn ghost" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="!ready" @click="submit">Compare</button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.cmp-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: flex-start;
  justify-content: center;
  padding: 6vh 16px;
  z-index: 50;
}
.cmp {
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 8px;
  width: min(640px, 100%);
  max-height: 88vh;
  display: flex;
  flex-direction: column;
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.35);
}
.cmp-head,
.cmp-foot {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 18px;
}
.cmp-head { border-bottom: 1px solid var(--datalib-border); }
.cmp-foot { border-top: 1px solid var(--datalib-border); justify-content: flex-end; }
.cmp-head h2 { margin: 0; font-size: 17px; flex: 1; }
.cmp-x {
  background: none;
  border: none;
  color: var(--datalib-muted);
  font-size: 22px;
  line-height: 1;
  cursor: pointer;
}
.cmp-body { padding: 16px 18px; overflow-y: auto; display: flex; flex-direction: column; gap: 14px; }
.cmp-blurb, .cmp-note { margin: 0; color: var(--datalib-muted); }
.cmp-error { margin: 0; color: #ef4444; }
.cmp-field { display: flex; flex-direction: column; gap: 4px; }
.cmp-label { font-weight: 600; font-size: 13px; }
.cmp-hint { color: var(--datalib-muted); font-size: 12px; }
.cmp-input {
  width: 100%;
  padding: 8px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
}
.cmp-num { width: 8em; }
</style>
