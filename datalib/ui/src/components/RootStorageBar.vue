<script setup lang="ts">
// The whole data root, in the status bar: its path, how much of the
// disk it takes, and how that has moved over the last few minutes.
// Not the sum of the sources — it includes `system/`, the
// stores, the served attachments, and anything a deleted step left
// behind. Read from `GET /api/pipeline/storage`, which the backend
// walks on a tick *while a sync runs* and otherwise on request.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { type PipelineStorage } from "@/api";
import { useApi } from "@/cards/cardApi";
import { formatBytes } from "@/config/bytes";
import { sparkline } from "@/config/sparkline";
import { changed, subscribeLive } from "@/live";
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { copyToClipboard } from "@/clipboard";
import { pushToast } from "@/toasts";
import { PATH_GLYPHS } from "@/config/glyphs";

const { fetchPipelineStorage } = useApi();

const storage = ref<PipelineStorage | null>(null);
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

/// The plot box, in user units.
const SPARK = { width: 160, height: 18 };

async function load(refresh = false) {
  try {
    storage.value = await fetchPipelineStorage(refresh);
  } catch {
    // The last answer stands; the bar is chrome, not a place for errors.
  }
}

const windowPhrase = computed(() => {
  const secs = storage.value?.window_secs ?? 300;
  return secs % 60 === 0
    ? `the last ${secs / 60} minute${secs === 60 ? "" : "s"}`
    : `the last ${secs} seconds`;
});

/// Scaled to its own range rather than to zero: five minutes of a sync
/// moves a large root by a fraction of a percent and would otherwise
/// draw flat. A series that hasn't moved straddles its value, so it
/// draws through the middle of the box rather than pinned to an edge.
const scale = computed(() => {
  const h = storage.value?.root.history ?? [];
  const values = h.map((x) => x.bytes);
  if (storage.value?.measured_at_utc) values.push(storage.value.root.bytes);
  if (values.length === 0) return { min: 0, max: 0 };
  const min = Math.min(...values);
  const max = Math.max(...values);
  if (min !== max) return { min, max };
  return min === 0 ? { min: 0, max: 1 } : { min: min * 0.99, max: max * 1.01 };
});

const delta = computed(() => {
  const h = storage.value?.root.history ?? [];
  if (h.length < 2 || !storage.value) return null;
  return storage.value.root.bytes - h[0].bytes;
});

const title = computed(() => {
  // A response whose `measured_at_utc` is null is a server that hasn't
  // finished its first walk. Its zero is not an empty disk.
  if (!storage.value?.measured_at_utc) return "Measuring the data root…";
  const now = formatBytes(storage.value.root.bytes);
  const moved = delta.value;
  if (moved === null || moved === 0) {
    return `${now} on disk. No change recorded over ${windowPhrase.value}.`;
  }
  // Said as a change rather than as two endpoints: both endpoints round
  // to the same figure whenever the movement is small against the total.
  return (
    `${now} on disk — ${moved > 0 ? "grew" : "shrank"} by ` +
    `${formatBytes(Math.abs(moved))} over ${windowPhrase.value}. The line is scaled ` +
    `to that change rather than to zero, so its height is the shape, not the size.`
  );
});

const sparkHost = ref<HTMLElement | null>(null);
function paint() {
  const host = sparkHost.value;
  if (!host) return;
  host.replaceChildren();
  const s = storage.value;
  if (!s) return;
  const spark = sparkline(
    s.root.history.map((h) => ({ at: h.at, value: h.bytes })),
    {
      nowMs: Date.now(),
      windowMs: s.window_secs * 1000,
      min: scale.value.min,
      max: scale.value.max,
      width: SPARK.width,
      height: SPARK.height,
      inset: 0.5,
    },
  );
  if (!spark) return;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${SPARK.width} ${SPARK.height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("root-spark");
  const area = document.createElementNS("http://www.w3.org/2000/svg", "polygon");
  area.setAttribute("points", spark.area);
  area.classList.add("root-spark-area");
  svg.appendChild(area);
  const line = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
  line.setAttribute("points", spark.line);
  line.classList.add("root-spark-line");
  svg.appendChild(line);
  host.appendChild(svg);
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
    <span class="root-bar-label">Data root</span>
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
    <span class="root-bar-size" :title="title">
      <b>{{ storage?.measured_at_utc ? formatBytes(storage.root.bytes) : "—" }}</b>
      <span v-if="delta !== null && delta !== 0" class="root-bar-delta">
        {{ delta > 0 ? "+" : "−" }}{{ formatBytes(Math.abs(delta)) }}
      </span>
    </span>
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
.root-bar-label {
  flex: 0 0 auto;
  font-weight: 600;
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
  margin-right: 4px;
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.root-bar-spark {
  flex: 0 0 auto;
  display: block;
  width: 160px;
  height: 18px;
}
.root-bar-size {
  flex: 0 0 auto;
  display: inline-flex;
  align-items: baseline;
  gap: 6px;
  font-variant-numeric: tabular-nums;
}
.root-bar-size b {
  color: var(--datalib-fg);
}
/* The change over the window, in the colour of the line that shows
   it. Not green: growth is not good news and shrinkage is not bad — the
   sign is the whole message. */
.root-bar-delta {
  color: var(--datalib-accent);
}
.root-bar-icon {
  flex: 0 0 auto;
  display: inline-flex;
  padding: 2px;
  border: none;
  border-radius: 4px;
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

<style>
/* The sparkline is built as plain DOM, so its classes can't be scoped.
   `vector-effect: non-scaling-stroke` is load-bearing: the svg is
   stretched from its 260-unit box to the span's width. */
.root-spark {
  display: block;
  overflow: visible;
  width: 100%;
  height: 100%;
}
.root-spark-line {
  fill: none;
  stroke: var(--datalib-accent);
  stroke-width: 1.25;
  stroke-linejoin: round;
  vector-effect: non-scaling-stroke;
}
.root-spark-area {
  fill: color-mix(in srgb, var(--datalib-accent) 22%, transparent);
  stroke: none;
}
</style>
