<script setup lang="ts">
// Lightweight sync indicator for the app header: a pulsing dot +
// "syncing" while any job is active, sitting in the header's flexible
// space so it never shifts the page layout. Per-job progress lives in
// the Sources tab; this only answers "is something running?". Click
// navigates there; the tooltip lists the active jobs.
import { computed, ref, onMounted, onUnmounted } from "vue";
import { useRouter } from "vue-router";
import {
  fetchActiveJobs,
  openJobStream,
  type SyncJob,
  type JobProgressEvent,
} from "@/api";

const router = useRouter();
// Active jobs, keyed by id for O(1) patching from the SSE stream.
const active = ref<Map<string, SyncJob>>(new Map());
let stream: EventSource | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;

const count = computed(() => active.value.size);
const tooltip = computed(() =>
  [...active.value.values()]
    .map((j) => `${j.source_name || "all"} (${j.kind})`)
    .join(", "),
);

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

function goSources() {
  router.push({ name: "sources" });
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
  <button
    v-if="count > 0"
    class="sync-indicator"
    :title="tooltip"
    @click="goSources"
  >
    <span class="dot" />
    syncing{{ count > 1 ? ` (${count})` : "" }}
  </button>
</template>

<style scoped>
.sync-indicator {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.15rem 0.6rem;
  margin-bottom: 0.45rem;
  border: 1px solid var(--fw-accent);
  border-radius: 9999px;
  background: transparent;
  color: var(--fw-accent);
  font-size: 0.78rem;
  cursor: pointer;
  white-space: nowrap;
}
.sync-indicator:hover {
  background: var(--fw-hover);
}
.dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: var(--fw-accent);
  animation: sync-pulse 1.2s ease-in-out infinite;
}
@keyframes sync-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.25;
  }
}
</style>
