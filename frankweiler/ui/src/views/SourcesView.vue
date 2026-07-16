<script setup lang="ts">
// The merged Setup + Sync tab: the sources table and the raw
// config.yaml editor sit side by side (stacking when the window is
// narrow) — not two tabs but two views of the same text. The editor is
// the single source of truth; the table re-derives from it on every
// keystroke. A row's Edit button selects that source's stanza in the
// editor; the chips append a template stanza and select it. Save PUTs
// the text to the backend, which validates with the real config loader
// before writing. Below all that, the recent-jobs table.
import { computed, nextTick, ref, onMounted, onUnmounted } from "vue";
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
import { listSources, type SourceRow } from "@/config/configSources";
import { SNIPPETS } from "@/config/snippets";

// --- Config state ----------------------------------------------------------

// The whole config.yaml text — the single source of truth.
const yamlText = ref("");
const editorEl = ref<HTMLTextAreaElement | null>(null);

const configPath = ref("");
const existed = ref(false);
const loadError = ref<string | null>(null);
// Result of the last Save attempt (null = unsaved edits or never saved).
const saveStatus = ref<{ ok: boolean; error: string | null; count: number } | null>(
  null,
);
const saving = ref(false);
const dirty = ref(false);
const latchkeyCli = ref("npx -y latchkey");

// Table view of the text: re-derived on every edit. While the text
// doesn't parse the last good rows stay up (grayed) with the parse
// error shown, so a half-typed edit doesn't blank the table.
const rows = ref<SourceRow[]>([]);
const parseError = ref<string | null>(null);

function reparse() {
  try {
    rows.value = listSources(yamlText.value);
    parseError.value = null;
  } catch (e) {
    parseError.value = (e as Error).message;
  }
}

function onEdit() {
  dirty.value = true;
  saveStatus.value = null;
  reparse();
}

// Saved-config source list from the backend — the sync-relevant view
// (`managed` is derived by the Rust loader, not the YAML). Keyed by
// name to decorate the table rows and gate the Sync buttons: sync runs
// against the file on disk, so a row is syncable only once the backend
// has seen it.
const serverSources = ref<SyncSource[]>([]);
const serverByName = computed(() => {
  const m = new Map<string, SyncSource>();
  for (const s of serverSources.value) m.set(s.name, s);
  return m;
});

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
    yamlText.value = cfg.yaml;
    reparse();
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

// Select [start, end) in the editor and scroll it into view. Textareas
// don't scroll to their selection on their own; estimate the target
// line's offset from the line count and the computed line height.
function selectRange(start: number, end: number) {
  nextTick(() => {
    const el = editorEl.value;
    if (!el) return;
    el.focus();
    el.setSelectionRange(start, end);
    const lineHeight = Number.parseFloat(getComputedStyle(el).lineHeight) || 16;
    const line = yamlText.value.slice(0, start).split("\n").length - 1;
    el.scrollTop = Math.max(0, line * lineHeight - el.clientHeight / 3);
  });
}

// Edit = jump to the stanza: select the row's slice of the text.
function selectSource(idx: number) {
  const r = rows.value[idx];
  if (!r) return;
  selectRange(r.start, r.end);
}

// Append a source stanza to the text and select it. If the YAML still
// has the scaffold's empty `sources: []`, flip it to a `sources:` block
// first so the appended item is valid.
function addSnippet(body: string) {
  let text = yamlText.value;
  if (/^sources:\s*\[\s*\]\s*$/m.test(text)) {
    text = text.replace(/^sources:\s*\[\s*\]\s*$/m, "sources:");
  } else if (!/^sources:/m.test(text)) {
    text = text.replace(/\s*$/, "") + "\n\nsources:";
  }
  const before = text.replace(/\s*$/, "") + "\n";
  yamlText.value = before + body + "\n";
  onEdit();
  selectRange(before.length, yamlText.value.length - 1);
}

async function onSave() {
  saving.value = true;
  saveStatus.value = null;
  try {
    const res = await saveConfig(yamlText.value);
    saveStatus.value = { ok: res.ok, error: res.error, count: res.source_count };
    if (res.ok) {
      dirty.value = false;
      existed.value = true;
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
    <h2>Configure data sources</h2>

    <p v-if="error" class="error">error: {{ error }}</p>
    <p v-if="loadError" class="error">Could not load config: {{ loadError }}</p>

    <p class="path">
      <span class="label">File:</span> <code>{{ configPath }}</code>
      <span v-if="!existed" class="pill new">not created yet</span>
    </p>

    <!-- Table and raw config side by side — two views of the same text.
         The columns wrap into a vertical stack when the window is too
         narrow for both. -->
    <div class="config-columns">
      <div class="table-col">
        <table class="sync-table sources-table" :class="{ stale: parseError }">
          <colgroup>
            <col class="col-name" />
            <col class="col-type" />
            <col class="col-flag" />
            <col class="col-flag" />
            <col class="col-actions" />
          </colgroup>
          <thead>
            <tr>
              <th>Name</th>
              <th>Type</th>
              <th>Enabled</th>
              <th>Managed</th>
              <th class="th-actions">
                <button
                  class="btn btn-sync"
                  :disabled="busyGlobal || serverSources.length === 0"
                  :title="serverSources.length === 0 ? 'Add a source first' : ''"
                  @click="syncEverything"
                >
                  {{ busyGlobal ? "Queuing…" : "Sync everything" }}
                </button>
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(r, idx) in rows"
              :key="idx"
              :class="{ 'row-disabled': !r.enabled }"
            >
              <td>{{ r.name || "(unnamed)" }}</td>
              <td>{{ r.type }}</td>
              <td>{{ r.enabled ? "yes" : "no" }}</td>
              <td>
                {{
                  serverByName.get(r.name)
                    ? serverByName.get(r.name)!.managed
                      ? "yes"
                      : "no"
                    : "—"
                }}
              </td>
              <td class="actions-cell src-actions">
                <button class="btn" title="Select this source in the config file" @click="selectSource(idx)">
                  Edit
                </button>
                <button
                  class="btn btn-sync"
                  :disabled="busySource[r.name] || !serverByName.get(r.name)"
                  :title="!serverByName.get(r.name) ? 'Not in the saved config yet' : ''"
                  @click="syncOne(r.name)"
                >
                  {{ busySource[r.name] ? "Queuing…" : "Sync" }}
                </button>
              </td>
            </tr>
            <tr v-if="rows.length === 0 && !loading">
              <td colspan="5" class="empty">
                no sources configured yet — add one with the buttons below.
              </td>
            </tr>
          </tbody>
        </table>

        <p v-if="parseError" class="status err">
          ✗ config has a YAML error (table may be stale): {{ parseError }}
        </p>

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
      </div>

      <div class="editor-col">
        <textarea
          ref="editorEl"
          v-model="yamlText"
          class="editor"
          spellcheck="false"
          autocomplete="off"
          autocapitalize="off"
          @input="onEdit"
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
      </div>
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
h2 {
  margin: 0;
  font-size: 1.1rem;
}
h3 {
  margin: 0.75rem 0 0.25rem;
  font-size: 0.95rem;
  color: var(--fw-muted);
  font-weight: 600;
}
.path {
  margin: 0;
  font-size: 0.85rem;
}
.label {
  color: var(--fw-muted);
  margin-right: 0.4rem;
}
/* Side-by-side columns that wrap into a vertical stack when the window
   can't fit both at a usable width. */
.config-columns {
  display: flex;
  flex-wrap: wrap;
  gap: 1rem;
  align-items: flex-start;
}
.table-col {
  flex: 3 1 26rem;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}
.editor-col {
  flex: 2 1 22rem;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
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
/* Fixed layout: column widths don't reflow with content. */
.sources-table {
  table-layout: fixed;
}
.sources-table.stale tbody {
  opacity: 0.6;
}
.sources-table .col-name {
  width: 22%;
}
.sources-table .col-type {
  width: 20%;
}
.sources-table .col-flag {
  width: 13%;
}
.sources-table .col-actions {
  width: 32%;
}
/* "Sync everything" lives in the header row, right-aligned so it lines
   up with the rows' Sync buttons. Undo the th's uppercase styling for
   the button label. */
.sources-table .th-actions {
  text-align: right;
}
.sources-table .th-actions .btn {
  text-transform: none;
  letter-spacing: normal;
  font-weight: 400;
}
.src-actions {
  justify-content: flex-end;
}
/* A little footprint stability for the "Sync" ↔ "Queuing…" label swap,
   without making the buttons look padded out. */
.src-actions .btn {
  min-width: 3.6rem;
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
.btn-cancel {
  color: #c0392b;
}
/* Accent-outlined sync actions ("Sync" and "Sync everything" match). */
.btn-sync {
  color: var(--fw-accent);
  border-color: var(--fw-accent);
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
.editor {
  width: 100%;
  min-height: 24rem;
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
