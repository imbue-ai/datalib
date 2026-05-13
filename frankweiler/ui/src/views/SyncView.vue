<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import {
  fetchSyncSources,
  fetchAllJobs,
  fetchActiveJobs,
  enqueueJob,
  cancelJob,
  type SyncSource,
  type SyncJob,
} from "@/api";

const sources = ref<SyncSource[]>([]);
const jobs = ref<SyncJob[]>([]);
const error = ref<string | null>(null);
const loading = ref(false);
const busySource = ref<Record<string, boolean>>({});
const busyGlobal = ref(false);

let pollTimer: ReturnType<typeof setInterval> | null = null;

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

async function refreshActive() {
  // Lightweight tick to surface progress updates without re-fetching 50 rows
  // every 2s. We merge active jobs into the existing list by id.
  try {
    const active = await fetchActiveJobs();
    if (active.length === 0) return;
    const byId = new Map(jobs.value.map((j) => [j.id, j]));
    let changed = false;
    for (const a of active) {
      const prev = byId.get(a.id);
      if (!prev || prev.state !== a.state || prev.progress_pct !== a.progress_pct) {
        changed = true;
      }
      byId.set(a.id, a);
    }
    if (changed) {
      // Refetch full list so newly-finished jobs slot in correctly.
      await loadJobs();
    }
  } catch {
    // best effort
  }
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

function pctText(j: SyncJob): string {
  if (j.progress_pct == null) return "";
  return `${Math.round(j.progress_pct * 100)}%`;
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
  pollTimer = setInterval(refreshActive, 2000);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});
</script>

<template>
  <section class="sync-view">
    <div class="sync-header">
      <h2>Sync</h2>
      <button
        class="btn btn-primary"
        :disabled="busyGlobal"
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
          <th>Provider</th>
          <th>Kind</th>
          <th>Managed</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="s in sources" :key="s.name">
          <td>{{ s.name }}</td>
          <td>{{ s.provider }}</td>
          <td>{{ s.kind }}</td>
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
          <td colspan="5" class="empty">no sources configured.</td>
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
        <tr v-for="j in jobs" :key="j.id">
          <td><code>{{ j.id.slice(0, 8) }}</code></td>
          <td>{{ j.kind }}</td>
          <td>{{ j.source_name || "" }}</td>
          <td>
            <span class="state-pill" :data-state="j.state">{{ j.state }}</span>
          </td>
          <td>
            <span :title="j.progress_msg || ''">{{ pctText(j) }}</span>
          </td>
          <td>{{ fmtTime(j.started_at) }}</td>
          <td>{{ fmtTime(j.finished_at) }}</td>
          <td>
            <button
              v-if="isActive(j)"
              class="btn btn-cancel"
              @click="onCancel(j)"
            >
              Cancel
            </button>
          </td>
        </tr>
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
