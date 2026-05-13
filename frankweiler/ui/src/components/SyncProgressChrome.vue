<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { useRouter } from "vue-router";
import { fetchActiveJobs, type SyncJob } from "@/api";

const router = useRouter();
const active = ref<SyncJob[]>([]);
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function poll() {
  try {
    active.value = await fetchActiveJobs();
  } catch {
    // best effort — chrome stays silent on errors
  }
}

function pctText(j: SyncJob): string {
  if (j.progress_pct == null) return "";
  return `${Math.round(j.progress_pct * 100)}%`;
}

function label(j: SyncJob): string {
  const src = j.source_name || "all";
  const pct = pctText(j);
  return pct ? `${src} (${j.kind}) ${pct}` : `${src} (${j.kind})`;
}

function goSync() {
  router.push({ name: "sync" });
}

onMounted(() => {
  poll();
  pollTimer = setInterval(poll, 2000);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});
</script>

<template>
  <div v-if="active.length > 0" class="sync-chrome" @click="goSync">
    <span
      v-for="j in active"
      :key="j.id"
      class="sync-pill"
      :title="j.progress_msg || ''"
    >
      {{ label(j) }}
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
  display: inline-block;
  padding: 0.1rem 0.55rem;
  border-radius: 9999px;
  background: var(--fw-input-bg);
  border: 1px solid var(--fw-accent);
  color: var(--fw-accent);
  font-size: 0.78rem;
  white-space: nowrap;
}
</style>
