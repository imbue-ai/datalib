<script setup lang="ts">
// Lightweight sync indicator for the toolbar: a pulsing dot +
// "syncing" while any request is open, sitting in the toolbar's flexible
// space so it never shifts the page layout. Per-row progress lives on
// the sources card; this only answers "is something running?". Click
// reveals that card; the tooltip lists the open requests and who opened
// each.
import { computed, ref, onMounted, onUnmounted } from "vue";
import { type SyncRequest } from "@/api";
import { useApi } from "@/cards/cardApi";
import { changed, subscribeLive } from "@/live";
import { showDataSources } from "@/surface";

const { fetchRequests } = useApi();

const open = ref<SyncRequest[]>([]);
let unsubscribe: (() => void) | null = null;

const count = computed(() => open.value.length);
const tooltip = computed(() =>
  open.value.map((r) => `${r.roots.join(", ")}${r.by === "ui" ? "" : ` (by ${r.by})`}`).join("; "),
);

async function load() {
  try {
    open.value = (await fetchRequests()).filter((r) => r.state === "open");
  } catch {
    // best effort — chrome stays silent on errors
  }
}

onMounted(() => {
  void load();
  // The requests live beside the loop's record, and a change to either
  // is a `dag` frame.
  unsubscribe = subscribeLive({
    root: (e) => {
      if (changed(e, "dag")) void load();
    },
    resync: load,
  });
});

onUnmounted(() => {
  unsubscribe?.();
  unsubscribe = null;
});
</script>

<template>
  <button v-if="count > 0" class="sync-indicator" :title="tooltip" @click="showDataSources">
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
  border: 1px solid var(--datalib-accent);
  border-radius: 9999px;
  background: transparent;
  color: var(--datalib-accent);
  font-size: 0.78rem;
  cursor: pointer;
  white-space: nowrap;
}
.sync-indicator:hover {
  background: var(--datalib-hover);
}
.dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: var(--datalib-accent);
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
