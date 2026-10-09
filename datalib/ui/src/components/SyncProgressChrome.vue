<script setup lang="ts">
// The toolbar's sync indicator, just left of the search box: a pulsing
// dot and how many sources are syncing, while any request is open. A
// click opens a menu of what is syncing, each with its Stop, and the way
// to the sources card, where the per-step progress is.
import { computed, onBeforeUnmount, onMounted, onUnmounted, ref, watch } from "vue";
import { type SyncRequest } from "@/api";
import { useApi } from "@/cards/cardApi";
import { changed, subscribeLive } from "@/live";
import { showDataSources } from "@/surface";
import { pushToast } from "@/toasts";
import { pillLabel, syncingGroups, type SyncingGroup } from "./syncPill";

const { fetchRequests, fetchManageRows, stopRequest } = useApi();

const requests = ref<SyncRequest[]>([]);
const groups = ref<SyncingGroup[]>([]);
const open = ref(false);
const el = ref<HTMLElement | null>(null);
let unsubscribe: (() => void) | null = null;

const label = computed(() => pillLabel(groups.value));

/// Only the newest answer is kept, so a slow one cannot bring back a
/// sync that has since ended.
let asked = 0;
async function load() {
  const mine = ++asked;
  try {
    const now = (await fetchRequests()).filter((r) => r.state === "open");
    const rows = now.length > 0 ? (await fetchManageRows()).rows : [];
    if (mine !== asked) return;
    requests.value = now;
    groups.value = syncingGroups(rows);
    if (now.length === 0) open.value = false;
  } catch {
    // best effort — chrome stays silent on errors
  }
}

async function stop(group: SyncingGroup) {
  try {
    await Promise.all(group.requestIds.map((id) => stopRequest(id)));
  } catch (e) {
    pushToast(`Could not stop ${group.name}: ${(e as Error).message}`, "error");
  }
}

function openSources() {
  open.value = false;
  showDataSources();
}

// `pointerdown` in the capture phase, because the top bar is a
// window-drag region whose handler takes the `mousedown` first.
function onDocDown(e: PointerEvent) {
  if (el.value && !el.value.contains(e.target as Node)) open.value = false;
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") open.value = false;
}
watch(open, (now) => {
  if (now) {
    document.addEventListener("pointerdown", onDocDown, true);
    window.addEventListener("keydown", onKey);
  } else {
    document.removeEventListener("pointerdown", onDocDown, true);
    window.removeEventListener("keydown", onKey);
  }
});
onBeforeUnmount(() => (open.value = false));

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
  <div v-if="requests.length > 0" ref="el" class="sync">
    <button
      class="sync-indicator"
      :class="{ 'is-open': open }"
      aria-haspopup="menu"
      :aria-expanded="open"
      @click="open = !open"
    >
      <span class="dot" />
      {{ label }}
    </button>
    <div v-if="open" class="sync-menu" role="menu">
      <ul v-if="groups.length" class="sync-list">
        <li v-for="g in groups" :key="g.id" class="sync-row">
          <span class="sync-name" :title="g.name">{{ g.name }}</span>
          <span v-if="g.progress" class="sync-progress">{{ g.progress }}</span>
          <button class="sync-link" role="menuitem" @click="stop(g)">Stop</button>
        </li>
      </ul>
      <button class="sync-link sync-all" role="menuitem" @click="openSources">Open Sources</button>
    </div>
  </div>
</template>

<style scoped>
.sync {
  position: relative;
  display: flex;
}
.sync-indicator {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  height: calc(var(--datalib-control-h) - 2px);
  box-sizing: border-box;
  padding: 0 0.6rem;
  border: 1px solid color-mix(in srgb, var(--datalib-accent) 45%, var(--datalib-border));
  border-radius: 9999px;
  background: var(--datalib-bg);
  color: var(--datalib-accent);
  font: inherit;
  font-size: var(--datalib-font-size-small);
  font-weight: 600;
  cursor: pointer;
  white-space: nowrap;
}
.sync-indicator:hover,
.sync-indicator.is-open {
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
.sync-menu {
  position: absolute;
  top: calc(100% + 4px);
  right: 0;
  z-index: 1000;
  width: 320px;
  max-width: calc(100vw - 20px);
  padding: 6px;
  display: flex;
  flex-direction: column;
  background: var(--datalib-bg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 2px);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.16);
  font-size: var(--datalib-font-size);
}
/* About eight rows, then it scrolls: a library can sync dozens at once. */
.sync-list {
  margin: 0 0 4px;
  padding: 0 0 4px;
  list-style: none;
  max-height: 15rem;
  overflow-y: auto;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.sync-row {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 5px 8px;
}
.sync-name {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sync-progress {
  flex: 0 0 auto;
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
  white-space: nowrap;
}
.sync-link {
  flex: 0 0 auto;
  padding: 0;
  border: none;
  background: none;
  color: var(--datalib-accent);
  font: inherit;
  cursor: pointer;
}
.sync-link:hover {
  text-decoration: underline;
}
.sync-all {
  align-self: flex-start;
  padding: 5px 8px;
}
</style>
