<script setup lang="ts">
// The merged Setup + Sync tab: one list of data sources that you can
// edit, add to, and sync, plus the recent-jobs table. The sources
// panel has two modes, like a markdown editor's WYSIWYG/source
// toggle:
//   * Table — one row per source with Edit/Remove/Sync, "add a source"
//     chips, and an "additional config options" box for the non-source
//     stanzas (data_root, defaults, qmd, …).
//   * Raw file — the whole `config.yaml` in a textarea.
// Both modes edit the same state; `configSplit.ts` bridges the two
// representations (comment-preserving). Edits are local until Save,
// which PUTs the reassembled YAML to the backend (it validates with
// the real config loader before persisting).
import { computed, ref, onMounted, onUnmounted } from "vue";
import {
  fetchConfig,
  fetchConfigScaffold,
  saveConfig,
  fetchSyncSources,
  fetchAllJobs,
  fetchJobLog,
  enqueueJob,
  cancelJob,
  openJobStream,
  type SyncSource,
  type SyncJob,
  type JobProgressEvent,
} from "@/api";
import StepProgress from "@/components/StepProgress.vue";
import {
  splitConfig,
  joinConfig,
  fragmentError,
  summarizeFragment,
} from "@/config/configSplit";
import { SNIPPETS } from "@/config/snippets";

// --- Config state ----------------------------------------------------------

const mode = ref<"table" | "raw">("table");
// Raw-mode truth: the whole config.yaml text.
const rawYaml = ref("");
// Table-mode truth: one YAML fragment per source + everything else.
const fragments = ref<string[]>([]);
const rest = ref("");

const configPath = ref("");
const existed = ref(false);
const loadError = ref<string | null>(null);
// Shown when switching raw → table fails because the text doesn't parse.
const modeError = ref<string | null>(null);
// Result of the last Save attempt (null = unsaved edits or never saved).
const saveStatus = ref<{ ok: boolean; error: string | null; count: number } | null>(
  null,
);
const saving = ref(false);
const dirty = ref(false);
const latchkeyCli = ref("npx -y latchkey");

// Per-source inline editor. `editingIdx` indexes into `fragments`;
// `editingIsNew` marks a just-added row so Cancel removes it again.
const editingIdx = ref<number | null>(null);
const editDraft = ref("");
const editError = ref<string | null>(null);
const editingIsNew = ref(false);

// Saved-config source list from the backend — the sync-relevant view
// (`managed` is derived by the Rust loader, not the YAML). Keyed by
// name to decorate the table rows.
const serverSources = ref<SyncSource[]>([]);
const serverByName = computed(() => {
  const m = new Map<string, SyncSource>();
  for (const s of serverSources.value) m.set(s.name, s);
  return m;
});

type Row = {
  idx: number;
  name: string;
  type: string;
  enabled: boolean;
  valid: boolean;
  managed: boolean | null; // null = not in the saved config (or invalid)
};

const rows = computed<Row[]>(() =>
  fragments.value.map((frag, idx) => {
    const sum = summarizeFragment(frag);
    return {
      idx,
      name: sum?.name ?? "(invalid)",
      type: sum?.type ?? "",
      enabled: sum?.enabled ?? true,
      valid: sum !== null,
      managed: sum ? (serverByName.value.get(sum.name)?.managed ?? null) : null,
    };
  }),
);

function markDirty() {
  dirty.value = true;
  saveStatus.value = null;
}

async function loadConfig() {
  loadError.value = null;
  try {
    let cfg = await fetchConfig();
    if (!cfg.exists) {
      // Fresh root — start from the server's scaffold so the user has a
      // valid base to add sources to.
      cfg = await fetchConfigScaffold();
    } else {
      existed.value = true;
    }
    configPath.value = cfg.path;
    if (cfg.latchkey_cli) latchkeyCli.value = cfg.latchkey_cli;
    rawYaml.value = cfg.yaml;
    try {
      const s = splitConfig(cfg.yaml);
      fragments.value = s.sources;
      rest.value = s.rest;
      mode.value = "table";
    } catch (e) {
      // Unparseable file on disk: open in raw mode so it can be fixed.
      mode.value = "raw";
      modeError.value = (e as Error).message;
    }
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

// The current full-file text for whichever mode holds the truth.
// Throws when table-mode state doesn't reassemble (bad "additional
// config options" YAML).
function currentYaml(): string {
  return mode.value === "raw" ? rawYaml.value : joinConfig(rest.value, fragments.value);
}

function setMode(m: "table" | "raw") {
  if (m === mode.value) return;
  modeError.value = null;
  if (m === "raw") {
    // Drop any open (uncommitted) source editor first, so a
    // never-Applied template doesn't get serialized into the raw text.
    cancelEdit();
    try {
      rawYaml.value = currentYaml();
    } catch (e) {
      modeError.value = (e as Error).message;
      return;
    }
    mode.value = "raw";
  } else {
    try {
      const s = splitConfig(rawYaml.value);
      fragments.value = s.sources;
      rest.value = s.rest;
      mode.value = "table";
    } catch (e) {
      modeError.value = `fix the YAML before switching to the table: ${(e as Error).message}`;
    }
  }
}

function startEdit(idx: number) {
  // Close any other open editor first; if it was an uncommitted
  // template row it gets removed, shifting later indices down by one.
  if (editingIdx.value !== null && editingIdx.value !== idx) {
    const removedBefore = editingIsNew.value && editingIdx.value < idx;
    cancelEdit();
    if (removedBefore) idx -= 1;
  }
  editingIdx.value = idx;
  editDraft.value = fragments.value[idx];
  editError.value = null;
  editingIsNew.value = false;
}

function cancelEdit() {
  if (editingIsNew.value && editingIdx.value !== null) {
    fragments.value.splice(editingIdx.value, 1);
  }
  editingIdx.value = null;
  editDraft.value = "";
  editError.value = null;
  editingIsNew.value = false;
}

function applyEdit() {
  if (editingIdx.value === null) return;
  const err = fragmentError(editDraft.value);
  if (err) {
    editError.value = err;
    return;
  }
  fragments.value[editingIdx.value] = editDraft.value;
  editingIdx.value = null;
  editDraft.value = "";
  editError.value = null;
  editingIsNew.value = false;
  markDirty();
}

function removeSource(idx: number) {
  // Removing the row that's open in the editor: if it's an uncommitted
  // template, cancelEdit() already removes it (and the config never
  // changed, so nothing to mark dirty).
  const wasNew = editingIsNew.value && editingIdx.value === idx;
  cancelEdit();
  if (wasNew) return;
  fragments.value.splice(idx, 1);
  markDirty();
}

// Chip click: append a template row and open it in the editor. Nothing
// is committed until Apply, so Cancel leaves the config untouched.
function addSnippet(body: string) {
  cancelEdit();
  fragments.value.push(body);
  editingIdx.value = fragments.value.length - 1;
  editDraft.value = body;
  editError.value = null;
  editingIsNew.value = true;
}

async function onSave() {
  saving.value = true;
  saveStatus.value = null;
  try {
    const yaml = currentYaml();
    const res = await saveConfig(yaml);
    saveStatus.value = { ok: res.ok, error: res.error, count: res.source_count };
    if (res.ok) {
      dirty.value = false;
      existed.value = true;
      rawYaml.value = yaml;
      await loadSources();
    }
  } catch (e) {
    saveStatus.value = { ok: false, error: (e as Error).message, count: 0 };
  } finally {
    saving.value = false;
  }
}

// --- Sync / jobs state (same behavior as the old Sync tab) ------------------

const jobs = ref<SyncJob[]>([]);
const error = ref<string | null>(null);
const loading = ref(false);
const busySource = ref<Record<string, boolean>>({});
const busyGlobal = ref(false);

// Per-job log viewer. `expandedId` is the job whose detail row is open;
// `logText`/`logError` hold the fetched tail. The backend serves it from
// `<root>/state/job-logs/<id>.log` via GET /api/sync/jobs/{id}/log.
const expandedId = ref<string | null>(null);
const logText = ref("");
const logError = ref<string | null>(null);
const logLoading = ref(false);

// Log lines with a severity class for the structured-JSON ones (the
// tracing subscriber emits NDJSON with a top-level `level`); qmd's and
// other plain-text lines pass through unhighlighted.
const logLines = computed(() =>
  logText.value.split("\n").map((text) => {
    let cls = "";
    if (text.startsWith("{")) {
      try {
        const level = JSON.parse(text)?.level;
        if (level === "ERROR") cls = "log-line-error";
        else if (level === "WARN") cls = "log-line-warn";
      } catch {
        // Not valid JSON after all — leave unhighlighted.
      }
    }
    return { text, cls };
  }),
);

let pollTimer: ReturnType<typeof setInterval> | null = null;
let stream: EventSource | null = null;
let reloadTimer: ReturnType<typeof setTimeout> | null = null;

async function loadSources() {
  try {
    serverSources.value = await fetchSyncSources();
  } catch (e) {
    error.value = (e as Error).message;
  }
}

async function loadJobs() {
  try {
    jobs.value = await fetchAllJobs(50);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

// Apply one SSE push. If the job is already in the list, patch it in
// place so the segmented bar updates without a full reload; otherwise
// (a brand-new job, or a terminal event that needs finished_at/error)
// schedule a debounced reload to pull the authoritative row.
function onProgress(ev: JobProgressEvent) {
  const j = jobs.value.find((x) => x.id === ev.id);
  const terminal = ev.state === "done" || ev.state === "failed" || ev.state === "canceled";
  if (j) {
    j.state = ev.state;
    j.progress_pct = ev.progress_pct;
    j.progress_msg = ev.progress_msg;
    // Terminal rows need server-stamped finished_at/error: reload soon.
    if (terminal) scheduleReload();
  } else {
    // Unknown job (just enqueued): bring it into the list.
    scheduleReload();
  }
  // Live-tail the open log while its job is still active.
  if (expandedId.value === ev.id && !terminal) {
    loadLog(ev.id);
  }
}

// Coalesce reloads so a burst of events (e.g. all-sources finishing)
// triggers a single fetch.
function scheduleReload() {
  if (reloadTimer) return;
  reloadTimer = setTimeout(() => {
    reloadTimer = null;
    loadJobs();
  }, 250);
}

// Reconnect fallback: SSE auto-reconnects, but if the page was
// backgrounded or the stream silently stalled we still want eventual
// consistency. A slow full reload covers the gap without the old
// sub-second hammering.
async function slowReload() {
  await loadJobs();
}

// "Sync now" enqueues against the *saved* config, so it's blocked while
// there are unsaved edits (the row might not exist / differ on disk).
const syncBlocked = computed(() => dirty.value);
const syncBlockedTitle = "Save your changes first";

async function syncOne(name: string) {
  busySource.value[name] = true;
  error.value = null;
  try {
    await enqueueJob({ kind: "all", source_name: name });
    await loadJobs();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busySource.value[name] = false;
  }
}

async function syncEverything() {
  busyGlobal.value = true;
  error.value = null;
  try {
    await enqueueJob({ kind: "all" });
    await loadJobs();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busyGlobal.value = false;
  }
}

async function onCancel(job: SyncJob) {
  try {
    await cancelJob(job.id);
    await loadJobs();
  } catch (e) {
    error.value = (e as Error).message;
  }
}

async function loadLog(id: string) {
  logLoading.value = true;
  logError.value = null;
  try {
    logText.value = await fetchJobLog(id);
  } catch (e) {
    // 404 = worker hasn't created the log file yet (job still pending).
    logText.value = "";
    logError.value = (e as Error).message.includes("404")
      ? "no log yet — the job hasn't started running."
      : (e as Error).message;
  } finally {
    logLoading.value = false;
  }
}

async function toggleLog(job: SyncJob) {
  if (expandedId.value === job.id) {
    expandedId.value = null;
    return;
  }
  expandedId.value = job.id;
  await loadLog(job.id);
}

function isActive(j: SyncJob): boolean {
  return j.state === "pending" || j.state === "running";
}

function fmtTime(s: string | null): string {
  if (!s) return "";
  // Trim seconds for terseness; keep original if parse fails.
  const d = new Date(s);
  if (isNaN(d.getTime())) return s;
  return d.toLocaleString();
}

onMounted(async () => {
  loading.value = true;
  await Promise.all([loadConfig(), loadSources(), loadJobs()]);
  loading.value = false;
  // Realtime push: the backend streams every job state change over SSE,
  // so progress moves the instant the worker writes it — no polling.
  stream = openJobStream(onProgress);
  // Reconnect/safety fallback: a slow full reload covers a silently
  // stalled stream (backgrounded tab, proxy timeout) without hammering.
  pollTimer = setInterval(slowReload, 15000);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
  if (reloadTimer) clearTimeout(reloadTimer);
  if (stream) stream.close();
});
</script>

<template>
  <section class="sources-view">
    <div class="view-header">
      <h2>Data sources</h2>
      <button
        class="btn btn-primary"
        :disabled="busyGlobal || syncBlocked || serverSources.length === 0"
        :title="
          syncBlocked
            ? syncBlockedTitle
            : serverSources.length === 0
              ? 'Add a source below first'
              : ''
        "
        @click="syncEverything"
      >
        {{ busyGlobal ? "Queuing…" : "Sync everything" }}
      </button>
    </div>

    <p v-if="error" class="error">error: {{ error }}</p>
    <p v-if="loadError" class="error">Could not load config: {{ loadError }}</p>

    <div class="config-head">
      <p class="path">
        <span class="label">File:</span> <code>{{ configPath }}</code>
        <span v-if="!existed" class="pill new">not created yet</span>
      </p>
      <div class="mode-toggle" role="tablist" aria-label="Config editing mode">
        <button
          class="mode-btn"
          :class="{ active: mode === 'table' }"
          role="tab"
          :aria-selected="mode === 'table'"
          @click="setMode('table')"
        >
          Table
        </button>
        <button
          class="mode-btn"
          :class="{ active: mode === 'raw' }"
          role="tab"
          :aria-selected="mode === 'raw'"
          @click="setMode('raw')"
        >
          Raw file
        </button>
      </div>
    </div>
    <p v-if="modeError" class="status err">✗ {{ modeError }}</p>

    <!-- Table mode -->
    <template v-if="mode === 'table'">
      <div class="snippets">
        <span class="label">Add a source:</span>
        <button
          v-for="sn in SNIPPETS"
          :key="sn.label"
          class="btn chip"
          @click="addSnippet(sn.body(latchkeyCli))"
        >
          + {{ sn.label }}
        </button>
      </div>

      <table class="sync-table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Type</th>
            <th>Enabled</th>
            <th>Managed</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          <template v-for="r in rows" :key="r.idx">
            <tr :class="{ 'row-disabled': !r.enabled }">
              <td>{{ r.name }}</td>
              <td>{{ r.type }}</td>
              <td>{{ r.enabled ? "yes" : "no" }}</td>
              <td>{{ r.managed === null ? "—" : r.managed ? "yes" : "no" }}</td>
              <td class="actions-cell">
                <button class="btn" @click="startEdit(r.idx)">
                  {{ editingIdx === r.idx ? "Editing…" : "Edit" }}
                </button>
                <button
                  class="btn"
                  :disabled="syncBlocked || busySource[r.name] || !serverByName.get(r.name)"
                  :title="
                    syncBlocked
                      ? syncBlockedTitle
                      : !serverByName.get(r.name)
                        ? 'Not in the saved config yet'
                        : ''
                  "
                  @click="syncOne(r.name)"
                >
                  {{ busySource[r.name] ? "Queuing…" : "Sync now" }}
                </button>
              </td>
            </tr>
            <tr v-if="editingIdx === r.idx" class="detail-row">
              <td colspan="5">
                <div class="edit-panel">
                  <textarea
                    v-model="editDraft"
                    class="editor editor-fragment"
                    spellcheck="false"
                    autocomplete="off"
                    autocapitalize="off"
                  />
                  <p v-if="editError" class="status err">✗ {{ editError }}</p>
                  <div class="edit-actions">
                    <button class="btn btn-primary" @click="applyEdit">Apply</button>
                    <button class="btn" @click="cancelEdit">Cancel</button>
                    <span class="spacer" />
                    <button class="btn btn-danger" @click="removeSource(r.idx)">
                      Remove source
                    </button>
                  </div>
                </div>
              </td>
            </tr>
          </template>
          <tr v-if="rows.length === 0 && !loading">
            <td colspan="5" class="empty">
              no sources configured yet — add one with the buttons above.
            </td>
          </tr>
        </tbody>
      </table>

      <details class="extra-config" :open="rest.trim() !== ''">
        <summary>
          Additional config options
          <span class="label">— every stanza other than <code>sources:</code>
            (<code>data_root</code>, <code>defaults</code>, <code>qmd</code>, …)</span>
        </summary>
        <textarea
          v-model="rest"
          class="editor editor-rest"
          spellcheck="false"
          autocomplete="off"
          autocapitalize="off"
          placeholder="# YAML stanzas other than sources:, e.g.
# defaults:
#   blob_size_limit_bytes: 5000000"
          @input="markDirty"
        />
      </details>
    </template>

    <!-- Raw mode -->
    <textarea
      v-else
      v-model="rawYaml"
      class="editor editor-raw"
      spellcheck="false"
      autocomplete="off"
      autocapitalize="off"
      @input="markDirty"
    />

    <div class="footer">
      <div class="save-status">
        <span v-if="saveStatus && saveStatus.ok" class="status ok">
          ✓ Saved — {{ saveStatus.count }} source(s) configured.
        </span>
        <span v-else-if="saveStatus && !saveStatus.ok" class="status err">
          ✗ Not saved: {{ saveStatus.error }}
        </span>
        <span v-else-if="dirty" class="status muted">unsaved changes</span>
      </div>
      <button class="btn btn-primary" :disabled="saving || !dirty" @click="onSave">
        {{ saving ? "Saving…" : "Save" }}
      </button>
    </div>

    <h3>Recent jobs</h3>
    <table class="sync-table">
      <thead>
        <tr>
          <th>ID</th>
          <th>Kind</th>
          <th>Source</th>
          <th>State</th>
          <th>Progress</th>
          <th>Started</th>
          <th>Finished</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        <template v-for="j in jobs" :key="j.id">
          <tr :class="{ 'row-failed': j.state === 'failed' }">
            <td><code>{{ j.id.slice(0, 8) }}</code></td>
            <td>{{ j.kind }}</td>
            <td>{{ j.source_name || "" }}</td>
            <td>
              <span class="state-pill" :data-state="j.state">{{ j.state }}</span>
            </td>
            <td class="progress-cell">
              <StepProgress :msg="j.progress_msg" :state="j.state" />
            </td>
            <td>{{ fmtTime(j.started_at) }}</td>
            <td>{{ fmtTime(j.finished_at) }}</td>
            <td class="actions-cell">
              <button class="btn btn-log" @click="toggleLog(j)">
                {{ expandedId === j.id ? "Hide log" : "Log" }}
              </button>
              <button
                v-if="isActive(j)"
                class="btn btn-cancel"
                @click="onCancel(j)"
              >
                Cancel
              </button>
            </td>
          </tr>
          <tr v-if="expandedId === j.id" class="detail-row">
            <td colspan="8">
              <div v-if="j.error" class="job-error">
                <strong>error:</strong> {{ j.error }}
              </div>
              <div class="log-head">
                <span>log <code>state/job-logs/{{ j.id }}.log</code></span>
                <button class="btn btn-mini" :disabled="logLoading" @click="loadLog(j.id)">
                  {{ logLoading ? "…" : "Refresh" }}
                </button>
              </div>
              <p v-if="logError" class="log-empty">{{ logError }}</p>
              <pre
                v-else-if="logText"
                class="log-body"
              ><span v-for="(l, i) in logLines" :key="i" :class="l.cls">{{ l.text + "\n" }}</span></pre>
              <p v-else class="log-empty">(empty)</p>
            </td>
          </tr>
        </template>
        <tr v-if="jobs.length === 0 && !loading">
          <td colspan="8" class="empty">no jobs yet.</td>
        </tr>
      </tbody>
    </table>
  </section>
</template>

<style scoped>
.sources-view {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 0 0.25rem;
}
.view-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
.view-header h2 {
  margin: 0;
  font-size: 1.1rem;
}
h3 {
  margin: 0.75rem 0 0.25rem;
  font-size: 0.95rem;
  color: var(--fw-muted);
  font-weight: 600;
}
.config-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
.path {
  margin: 0;
  font-size: 0.85rem;
}
.label {
  color: var(--fw-muted);
  margin-right: 0.4rem;
}
.mode-toggle {
  display: flex;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  overflow: hidden;
}
.mode-btn {
  background: var(--fw-input-bg);
  color: var(--fw-muted);
  border: none;
  padding: 0.25rem 0.7rem;
  font-size: 0.8rem;
  cursor: pointer;
}
.mode-btn + .mode-btn {
  border-left: 1px solid var(--fw-border);
}
.mode-btn.active {
  background: var(--fw-accent);
  color: white;
}
.snippets {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.4rem;
}
.sync-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.9rem;
  background: var(--fw-card-bg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  overflow: hidden;
}
.sync-table th,
.sync-table td {
  text-align: left;
  padding: 0.4rem 0.6rem;
  border-bottom: 1px solid var(--fw-border);
}
.sync-table th {
  background: var(--fw-hover);
  font-size: 0.8rem;
  text-transform: uppercase;
  letter-spacing: 0.02em;
  color: var(--fw-muted);
}
.sync-table tr:last-child td {
  border-bottom: none;
}
.row-disabled td:first-child,
.row-disabled td:nth-child(2) {
  color: var(--fw-muted);
}
.btn {
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  padding: 0.3rem 0.65rem;
  font-size: 0.85rem;
  cursor: pointer;
}
.btn:hover:not(:disabled) {
  background: var(--fw-hover);
}
.btn:disabled {
  opacity: 0.55;
  cursor: default;
}
.btn.chip {
  font-size: 0.78rem;
  padding: 0.2rem 0.5rem;
}
.btn-primary {
  background: var(--fw-accent);
  border-color: var(--fw-accent);
  color: white;
}
.btn-primary:hover:not(:disabled) {
  /* Re-assert the accent background: `.btn:hover:not(:disabled)` above
     has equal specificity and would otherwise paint the generic light
     hover background under this button's white text. */
  background: var(--fw-accent);
  filter: brightness(1.08);
}
.btn-danger {
  color: #c0392b;
}
.btn-cancel {
  color: #c0392b;
}
.btn-log {
  font-size: 0.78rem;
  padding: 0.2rem 0.5rem;
}
.btn-mini {
  font-size: 0.72rem;
  padding: 0.1rem 0.4rem;
}
.progress-cell {
  min-width: 14rem;
}
.actions-cell {
  display: flex;
  gap: 0.4rem;
}
.row-failed .state-pill[data-state="failed"] {
  font-weight: 600;
}
.detail-row td {
  background: var(--fw-code-bg);
}
.edit-panel {
  display: flex;
  flex-direction: column;
  gap: 0.4rem;
}
.edit-actions {
  display: flex;
  gap: 0.5rem;
}
.edit-actions .spacer {
  flex: 1;
}
.editor {
  width: 100%;
  box-sizing: border-box;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.82rem;
  line-height: 1.5;
  tab-size: 2;
  padding: 0.75rem;
  border: 1px solid var(--fw-border);
  border-radius: 6px;
  background: var(--fw-code-bg);
  color: var(--fw-fg);
  resize: vertical;
}
.editor-fragment {
  min-height: 10rem;
  background: var(--fw-bg);
}
.editor-rest {
  min-height: 7rem;
}
.editor-raw {
  min-height: 24rem;
}
.extra-config summary {
  cursor: pointer;
  font-size: 0.9rem;
  margin-bottom: 0.35rem;
}
.footer {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 1rem;
}
.save-status {
  flex: 1;
}
.pill {
  display: inline-block;
  margin-left: 0.5rem;
  font-size: 0.72rem;
  padding: 0.08rem 0.45rem;
  border-radius: 9999px;
  border: 1px solid var(--fw-border);
}
.pill.new {
  color: var(--fw-muted);
}
.status {
  font-size: 0.85rem;
}
.status.ok {
  color: #2e8b57;
}
.status.err {
  color: #c0392b;
}
.status.muted {
  color: var(--fw-muted);
}
.job-error {
  color: #c0392b;
  font-size: 0.82rem;
  margin-bottom: 0.5rem;
  white-space: pre-wrap;
  word-break: break-word;
}
.log-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem;
  font-size: 0.78rem;
  color: var(--fw-muted);
  margin-bottom: 0.35rem;
}
.log-body {
  margin: 0;
  max-height: 22rem;
  overflow: auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.74rem;
  line-height: 1.45;
  white-space: pre-wrap;
  word-break: break-word;
  background: var(--fw-bg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  padding: 0.5rem 0.6rem;
}
.log-line-error {
  color: var(--fw-log-error);
}
.log-line-warn {
  color: var(--fw-log-warn);
}
.log-empty {
  margin: 0;
  font-size: 0.8rem;
  color: var(--fw-muted);
  font-style: italic;
}
.state-pill {
  display: inline-block;
  font-size: 0.75rem;
  padding: 0.1rem 0.5rem;
  border-radius: 9999px;
  border: 1px solid var(--fw-border);
  background: var(--fw-input-bg);
  text-transform: uppercase;
  letter-spacing: 0.03em;
}
.state-pill[data-state="running"] {
  border-color: var(--fw-accent);
  color: var(--fw-accent);
}
.state-pill[data-state="done"] {
  color: #2e8b57;
  border-color: #2e8b57;
}
.state-pill[data-state="failed"] {
  color: #c0392b;
  border-color: #c0392b;
}
.state-pill[data-state="canceled"] {
  color: var(--fw-muted);
}
.empty {
  color: var(--fw-muted);
  font-style: italic;
  text-align: center;
}
.error {
  color: #e35d6a;
}
code {
  background: var(--fw-code-bg);
  padding: 0 0.25rem;
  border-radius: 2px;
  font-size: 0.8rem;
}
</style>
