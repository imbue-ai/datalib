<script setup lang="ts">
// Arbitrary-JS visualization column. The card has two pieces of URL-
// persisted state — a search query (`q`, supplies `rows`) and a content
// hash (`js`) pointing at a backend-stored JS body. The JS runs inside
// a sandboxed iframe; rows are posted in via `postMessage`. Edits never
// touch the iframe directly — we rebuild the srcdoc on Run / Save.
import { ref, watch, onMounted, onUnmounted, computed } from "vue";
import { fetchSearch, createCard, fetchCard, type SearchRow } from "@/api";
import { buildCardSrcdoc } from "@/components/cardSandbox";

const props = defineProps<{
  q: string;
  js: string | null;
}>();

const emit = defineEmits<{
  (e: "update:q", q: string): void;
  (e: "update:js", js: string | null): void;
}>();

const STARTER = `// rows: SearchRow[] from /api/search?q=…
// mount: HTMLElement to render into
const h = document.createElement('h3');
h.textContent = rows.length + ' rows';
mount.appendChild(h);
for (const r of rows.slice(0, 20)) {
  const div = document.createElement('div');
  div.style.padding = '4px 0';
  div.style.borderBottom = '1px solid #ccc';
  div.textContent = (r.when || '') + ' · ' + (r.snippet || '');
  mount.appendChild(div);
}
`;

const query = ref(props.q);
const rows = ref<SearchRow[]>([]);
const loading = ref(false);
const fetchError = ref<string | null>(null);

// The in-memory truth for the JS body. `savedHash` tracks what the
// backend has on disk; `source !== <text loaded from savedHash>` means
// the editor is dirty.
const source = ref<string>("");
const savedHash = ref<string | null>(props.js);
const savedSource = ref<string>(""); // mirror of what `savedHash` resolves to

// Edit mode is forced when no js is set yet (blank card). Otherwise
// the user toggles it from the header.
const editing = ref<boolean>(props.js == null);

const dirty = computed(() => source.value !== savedSource.value);

// `runSource` is the snapshot the iframe is currently running. It
// only updates on Run / Save — not on every editor keystroke — so
// typing in the textarea doesn't trigger a srcdoc rebuild + iframe
// reload on each character.
const runSource = ref<string>("");
const runRev = ref(0);

const iframeReady = ref(false);
const iframeRef = ref<HTMLIFrameElement | null>(null);

const srcdoc = computed(() => buildCardSrcdoc(runSource.value));

// Push the current rows into the iframe. Called after every (rows, rev)
// change once the iframe has signaled `ready`.
//
// Vue wraps the array (and each row object) in reactive Proxies, and
// `postMessage`'s structured-clone algorithm refuses to serialize a
// Proxy — it throws DataCloneError. JSON-round-tripping yields a plain
// snapshot, which is also what we want semantically (the iframe sees
// the rows at message-send time, not a live reference).
function pushRows() {
  if (!iframeReady.value) return;
  const w = iframeRef.value?.contentWindow;
  if (!w) return;
  const plainRows = JSON.parse(JSON.stringify(rows.value));
  w.postMessage({ type: "rows", rows: plainRows }, "*");
}

function onIframeMessage(ev: MessageEvent) {
  // The sandboxed iframe has a null origin, so we can't filter by
  // ev.origin meaningfully. The shape check is enough — the payload
  // doesn't carry trust.
  if (!ev.data) return;
  if (ev.data.type === "ready" && ev.source === iframeRef.value?.contentWindow) {
    iframeReady.value = true;
    pushRows();
  }
}

let inflight: AbortController | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;

async function runSearch(q: string) {
  inflight?.abort();
  inflight = new AbortController();
  loading.value = true;
  fetchError.value = null;
  try {
    const r = await fetchSearch(q, 1000, inflight.signal);
    rows.value = r.rows;
    pushRows();
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    fetchError.value = (e as Error).message;
  } finally {
    loading.value = false;
  }
}

watch(query, (q) => {
  if (debounceTimer) clearTimeout(debounceTimer);
  debounceTimer = setTimeout(() => runSearch(q), 150);
  if (q !== props.q) emit("update:q", q);
});

watch(
  () => props.q,
  (q) => {
    if (q !== query.value) query.value = q;
  },
);

// When the URL hash changes externally (back/forward), reload the
// source from the backend and discard any unsaved editor state.
watch(
  () => props.js,
  async (next) => {
    if (next === savedHash.value) return;
    if (next == null) {
      savedHash.value = null;
      savedSource.value = "";
      source.value = STARTER;
      runSource.value = STARTER;
      editing.value = true;
      runRev.value++;
      return;
    }
    try {
      const body = await fetchCard(next);
      savedHash.value = next;
      savedSource.value = body;
      source.value = body;
      runSource.value = body;
      editing.value = false;
      runRev.value++;
    } catch (e) {
      fetchError.value = `load card: ${(e as Error).message}`;
    }
  },
);

async function onSave() {
  try {
    const hash = await createCard(source.value);
    savedHash.value = hash;
    savedSource.value = source.value;
    runSource.value = source.value;
    iframeReady.value = false;
    runRev.value++;
    if (hash !== props.js) emit("update:js", hash);
    editing.value = false;
  } catch (e) {
    fetchError.value = `save card: ${(e as Error).message}`;
  }
}

function onRun() {
  // Preview without saving — snapshot the editor's current text and
  // rebuild srcdoc against it. The js hash in the URL doesn't change.
  runSource.value = source.value;
  iframeReady.value = false;
  runRev.value++;
}

function toggleEdit() {
  editing.value = !editing.value;
}

onMounted(async () => {
  if (props.js) {
    try {
      const body = await fetchCard(props.js);
      savedSource.value = body;
      source.value = body;
      runSource.value = body;
    } catch (e) {
      fetchError.value = `load card: ${(e as Error).message}`;
      source.value = STARTER;
      runSource.value = STARTER;
      editing.value = true;
    }
  } else {
    source.value = STARTER;
    runSource.value = STARTER;
  }
  window.addEventListener("message", onIframeMessage);
  await runSearch(query.value);
});

onUnmounted(() => {
  window.removeEventListener("message", onIframeMessage);
});

// Re-push rows on every successful run.
watch([rows, runRev], () => pushRows());
</script>

<template>
  <div class="card-column">
    <div class="card-header">
      <input
        v-model="query"
        class="card-query"
        placeholder="card query (matches /api/search)"
      />
      <button type="button" class="card-btn" @click="toggleEdit">
        {{ editing ? "Hide editor" : "Edit" }}
      </button>
      <button
        type="button"
        class="card-btn"
        :disabled="!editing"
        @click="onRun"
      >
        Run
      </button>
      <button
        type="button"
        class="card-btn card-btn-primary"
        :disabled="!dirty"
        @click="onSave"
      >
        Save
      </button>
    </div>
    <p v-if="fetchError" class="card-error">{{ fetchError }}</p>
    <p class="card-meta">
      <span v-if="loading">loading…</span>
      <span v-else>{{ rows.length }} rows</span>
      <span v-if="savedHash" class="card-hash" :title="savedHash">
        · saved <code>{{ savedHash.slice(0, 8) }}</code>
      </span>
      <span v-else class="card-hash">· unsaved</span>
      <span v-if="dirty" class="card-dirty">· modified</span>
    </p>
    <div v-if="editing" class="card-editor">
      <textarea
        v-model="source"
        class="card-textarea"
        spellcheck="false"
        autocomplete="off"
      />
    </div>
    <div class="card-iframe-wrap">
      <iframe
        :key="runRev"
        ref="iframeRef"
        class="card-iframe"
        sandbox="allow-scripts"
        :srcdoc="srcdoc"
      />
    </div>
  </div>
</template>

<style scoped>
.card-column {
  display: flex;
  flex-direction: column;
  height: 100%;
  gap: 0.4rem;
  padding: 0.5rem;
  box-sizing: border-box;
}
.card-header {
  display: flex;
  gap: 0.3rem;
  align-items: center;
}
.card-query {
  flex: 1;
  padding: 0.35rem 0.5rem;
  font-size: 0.9rem;
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
}
.card-btn {
  padding: 0.35rem 0.6rem;
  font-size: 0.85rem;
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  cursor: pointer;
}
.card-btn:hover:not(:disabled) {
  background: var(--fw-hover);
}
.card-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.card-btn-primary {
  border-color: var(--fw-accent);
  color: var(--fw-accent);
}
.card-meta {
  margin: 0;
  font-size: 0.8rem;
  color: var(--fw-muted);
}
.card-hash code {
  background: var(--fw-code-bg);
  padding: 0 0.2rem;
  border-radius: 2px;
}
.card-dirty {
  color: #d18a3a;
  margin-left: 0.3rem;
}
.card-error {
  margin: 0;
  padding: 0.3rem 0.5rem;
  border: 1px solid #e35d6a;
  border-radius: 4px;
  background: rgba(227, 93, 106, 0.1);
  color: #e35d6a;
  font-size: 0.85rem;
}
.card-editor {
  flex: 0 0 40%;
  min-height: 120px;
  display: flex;
}
.card-textarea {
  flex: 1;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  padding: 0.4rem;
  background: var(--fw-code-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  resize: none;
}
.card-iframe-wrap {
  flex: 1 1 auto;
  min-height: 100px;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  background: var(--fw-bg);
  overflow: hidden;
}
.card-iframe {
  width: 100%;
  height: 100%;
  border: none;
}
</style>
