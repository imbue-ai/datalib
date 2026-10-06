<script setup lang="ts">
// The whole data root, in the status bar: its path, how much of the
// disk it takes, and how that has moved over the last few minutes.
// Not the sum of the sources — it includes `system/`, the
// stores, the served attachments, and anything a deleted step left
// behind. Read from `GET /api/pipeline/storage`, which the backend
// walks on a tick *while a sync runs* and otherwise on request.
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { type PipelineStorage } from "@/api";
import { useApi } from "@/cards/cardApi";
import { formatBytes } from "@/config/bytes";
import { sparkTrack } from "@/cards/cellRenderers";
// The plot's classes, shared with the Manage table's size column. Every
// one is `tg-` prefixed, so loading it here styles nothing else.
import "@/cards/tableGrid.css";
import { changed, subscribeLive } from "@/live";
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { copyToClipboard } from "@/clipboard";
import { pushToast } from "@/toasts";
import { PATH_GLYPHS } from "@/config/glyphs";

const { fetchPipelineStorage } = useApi();

const storage = ref<PipelineStorage | null>(null);
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

async function load(refresh = false) {
  try {
    storage.value = await fetchPipelineStorage(refresh);
  } catch {
    // The last answer stands; the bar is chrome, not a place for errors.
  }
}

const title = ref("Measuring the data root…");
const sparkHost = ref<HTMLElement | null>(null);
function paint() {
  const host = sparkHost.value;
  if (!host) return;
  host.replaceChildren();
  const s = storage.value;
  // A response whose `measured_at_utc` is null is a server that hasn't
  // finished its first walk. Its zero is not an empty disk.
  if (!s?.measured_at_utc) {
    title.value = "Measuring the data root…";
    host.textContent = "—";
    return;
  }
  const track = sparkTrack(
    s.root.bytes,
    "bytes",
    s.root.history.map((h) => ({ at: h.at, value: h.bytes })),
    s.window_secs,
  );
  title.value = `${formatBytes(s.root.bytes)} on disk.\n${track.change}`;
  host.appendChild(track.el);
}
watch([storage, sparkHost], paint, { flush: "post" });

async function reveal() {
  if (storage.value) await revealInFileManager(storage.value.root.abs);
}

async function copyPath() {
  if (!storage.value) return;
  const ok = await copyToClipboard(storage.value.root.abs);
  pushToast(ok ? "Data root path copied" : "Could not copy the path", ok ? "info" : "error");
}

let unsubscribe: (() => void) | null = null;
onMounted(() => {
  // Fresh on the first paint: the backend only walks on its own while
  // a run holds the root, so on an idle root its last answer can be old.
  void load(true);
  unsubscribe = subscribeLive({
    root: (e) => {
      // The sampler says when it has walked; there is nothing new to
      // read between its samples.
      if (changed(e, "storage")) void load();
    },
    resync: () => void load(true),
  });
});
onBeforeUnmount(() => unsubscribe?.());
</script>

<template>
  <div class="root-bar" data-testid="root-storage">
    <span class="root-bar-where">
      <code class="root-bar-path" :title="storage?.root.abs ?? ''">{{ storage?.root.abs }}</code>
      <button
        v-if="canReveal && storage"
        class="root-bar-icon"
        :title="`${revealLabel} — the data root itself`"
        :aria-label="revealLabel"
        @click="reveal"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path :d="PATH_GLYPHS.reveal" fill="currentColor" />
        </svg>
      </button>
      <button
        v-if="storage"
        class="root-bar-icon"
        title="Copy the data root's path"
        aria-label="Copy path"
        @click="copyPath"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path :d="PATH_GLYPHS.copy" fill="currentColor" />
        </svg>
      </button>
    </span>
    <span class="root-bar-spark" ref="sparkHost" :title="title"></span>
  </div>
</template>

<style scoped>
/* Shrinks with the row: the path gives way first (below). */
.root-bar {
  flex: 0 1 auto;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 12px;
  color: var(--datalib-muted);
}
/* The path and its buttons, kept together; the path yields first when
   the window narrows — the number and the plot are the point of the line. */
.root-bar-where {
  flex: 0 1 auto;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 2px;
}
.root-bar-path {
  font-family: var(--datalib-mono);
  font-size: var(--datalib-font-size-small);
  margin-right: 4px;
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.root-bar-spark {
  flex: 0 0 auto;
  display: flex;
  width: 200px;
}
.root-bar-icon {
  flex: 0 0 auto;
  display: inline-flex;
  padding: 2px;
  border: none;
  border-radius: var(--datalib-radius);
  background: none;
  color: inherit;
  cursor: pointer;
}
.root-bar-icon svg {
  width: 14px;
  height: 14px;
}
.root-bar-icon:hover {
  background: var(--datalib-hover);
  color: var(--datalib-fg);
}
</style>
