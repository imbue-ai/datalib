<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { useRouter } from "vue-router";
import {
  fetchActiveJobs,
  openJobStream,
  type SyncJob,
  type JobProgressEvent,
} from "@/api";
import StepProgress from "@/components/StepProgress.vue";

const router = useRouter();
// Active jobs, keyed by id for O(1) patching from the SSE stream.
const active = ref<Map<string, SyncJob>>(new Map());
let stream: EventSource | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;

function activeList(): SyncJob[] {
  return [...active.value.values()];
}

// Seed from the API (covers jobs already running when this mounts), then
// keep current via SSE push.
async function seed() {
  try {
    const list = await fetchActiveJobs();
    const m = new Map<string, SyncJob>();
    for (const j of list) m.set(j.id, j);
    active.value = m;
  } catch {
    // best effort — chrome stays silent on errors
  }
}

function onProgress(ev: JobProgressEvent) {
  const m = active.value;
  const terminal = ev.state === "done" || ev.state === "failed" || ev.state === "canceled";
  if (terminal) {
    m.delete(ev.id);
  } else {
    const prev = m.get(ev.id);
    if (prev) {
      prev.state = ev.state;
      prev.progress_pct = ev.progress_pct;
      prev.progress_msg = ev.progress_msg;
    } else {
      // Newly-started job we haven't seen: pull the full active set so it
      // shows up with its kind/source fields.
      seed();
      return;
    }
  }
  // Reassign to trigger reactivity on the Map.
  active.value = new Map(m);
}

function label(j: SyncJob): string {
  const src = j.source_name || "all";
  return `${src} (${j.kind})`;
}

function goSync() {
  router.push({ name: "sync" });
}

onMounted(() => {
  seed();
  stream = openJobStream(onProgress);
  // Slow reconnect/stall fallback (SSE is the primary path).
  pollTimer = setInterval(seed, 15000);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
  if (stream) stream.close();
});
</script>

<template>
  <div v-if="activeList().length > 0" class="sync-chrome" @click="goSync">
    <span
      v-for="j in activeList()"
      :key="j.id"
      class="sync-pill"
      :title="j.progress_msg || ''"
    >
      <span class="pill-label">{{ label(j) }}</span>
      <span class="pill-bar">
        <StepProgress :msg="j.progress_msg" :state="j.state" />
      </span>
    </span>
  </div>
</template>

<style scoped>
.sync-chrome {
  display: flex;
  gap: 0.4rem;
  align-items: center;
  flex-wrap: wrap;
  padding: 0.25rem 0.5rem;
  background: var(--fw-card-bg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  cursor: pointer;
  font-size: 0.8rem;
  margin-bottom: 0.5rem;
}
.sync-chrome:hover {
  background: var(--fw-hover);
}
.sync-pill {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.1rem 0.55rem;
  border-radius: 9999px;
  background: var(--fw-input-bg);
  border: 1px solid var(--fw-accent);
  color: var(--fw-accent);
  font-size: 0.78rem;
  white-space: nowrap;
}
.pill-label {
  font-weight: 600;
}
/* Constrain the embedded StepProgress so the pill stays compact. */
.pill-bar {
  display: inline-block;
  width: 11rem;
}
</style>
