<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import {
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

const sources = ref<SyncSource[]>([]);
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

let pollTimer: ReturnType<typeof setInterval> | null = null;
let stream: EventSource | null = null;
let reloadTimer: ReturnType<typeof setTimeout> | null = null;

async function loadSources() {
  try {
    sources.value = await fetchSyncSources();
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

async function syncOne(src: SyncSource) {
  busySource.value[src.name] = true;
  error.value = null;
  try {
    await enqueueJob({ kind: "all", source_name: src.name });
    await loadJobs();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busySource.value[src.name] = false;
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
  await Promise.all([loadSources(), loadJobs()]);
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
  <section class="sync-view">
    <div class="sync-header">
      <h2>Sync</h2>
      <button
        class="btn btn-primary"
        :disabled="busyGlobal || sources.length === 0"
        :title="sources.length === 0 ? 'Add sources in Setup first' : ''"
        @click="syncEverything"
      >
        {{ busyGlobal ? "Queuing…" : "Sync everything" }}
      </button>
    </div>

    <p v-if="error" class="error">error: {{ error }}</p>

    <h3>Sources</h3>
    <table class="sync-table">
      <thead>
        <tr>
          <th>Name</th>
          <th>Type</th>
          <th>Managed</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="s in sources" :key="s.name">
          <td>{{ s.name }}</td>
          <td>{{ s.type }}</td>
          <td>{{ s.managed ? "yes" : "no" }}</td>
          <td>
            <button
              class="btn"
              :disabled="busySource[s.name]"
              @click="syncOne(s)"
            >
              {{ busySource[s.name] ? "Queuing…" : "Sync now" }}
            </button>
          </td>
        </tr>
        <tr v-if="sources.length === 0 && !loading">
          <td colspan="4" class="empty">
            no sources configured yet —
            <RouterLink to="/setup">set up your config</RouterLink> to add some.
          </td>
        </tr>
      </tbody>
    </table>

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
              <pre v-else-if="logText" class="log-body">{{ logText }}</pre>
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
.sync-view {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  padding: 0 0.25rem;
}
.sync-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
.sync-header h2 {
  margin: 0;
  font-size: 1.1rem;
}
h3 {
  margin: 0.75rem 0 0.25rem;
  font-size: 0.95rem;
  color: var(--fw-muted);
  font-weight: 600;
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
.btn-primary {
  background: var(--fw-accent);
  border-color: var(--fw-accent);
  color: white;
}
.btn-primary:hover:not(:disabled) {
  filter: brightness(1.08);
  background: var(--fw-accent);
}
.btn-cancel {
  color: #c0392b;
}
.btn-log {
  font-size: 0.78rem;
  padding: 0.2rem 0.5rem;
}
.progress-cell {
  min-width: 14rem;
}
.btn-mini {
  font-size: 0.72rem;
  padding: 0.1rem 0.4rem;
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
