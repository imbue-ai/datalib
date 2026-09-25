<script setup lang="ts">
// The embedding map as a card: every embedded document as a point,
// placed by the `embedding_map` step so documents qmd embeds alike sit
// together. A search bar with the grid's grammar lights up what it
// matches, a legend colours and isolates by one field, hovering shows
// a preview, and a click opens the document beside the card. What the
// card decides is in `embeddingMap.ts`; this file draws and listens.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { EMBEDDING_MAP_STEP, type DagStep, type EmbeddingMapResponse, type MapPoint } from "@/api";
import { subscribeLive, changed } from "@/live";
import { confirmAction } from "@/desktop";
import { useApi } from "./cardApi";
import { isBrowserClick } from "./chatLink";
import {
  COLOR_BY,
  OTHER,
  OTHER_COLOR,
  PointGrid,
  SLOTS,
  STEP_STANZA,
  categoryOf,
  decodeState,
  encodeState,
  fitView,
  legendFor,
  previewText,
  slotOf,
  toData,
  toScreen,
  zoomAt,
  type ColorBy,
  type View,
} from "./embeddingMap";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; opts: { q?: string; by?: string } }>();
const api = useApi();

const saved = decodeState(props.ctx.initialState);
const query = ref(props.ctx.initialState ? saved.q : (props.opts.q ?? ""));
const applied = ref(query.value);
const by = ref<ColorBy>(
  props.ctx.initialState ? saved.by : decodeState(`by=${props.opts.by ?? ""}`).by,
);
const selected = ref<string | null>(saved.sel);

const map = shallowRef<EmbeddingMapResponse | null>(null);
/// What the filter matched; null when there is no filter.
const matches = shallowRef<Set<string> | null>(null);
const filtering = ref(false);
const loadError = ref<string | null>(null);
const loading = ref(false);
const step = shallowRef<DagStep | null>(null);
const configured = ref<boolean | null>(null);
const busy = ref<string | null>(null);

const hidden = ref(new Set<string>());
const focusKey = ref<string | null>(null);
const hovered = ref<number>(-1);
const tip = ref<{ x: number; y: number } | null>(null);
const previews = new Map<string, string>();
const preview = ref<string | null>(null);

const wrap = ref<HTMLElement | null>(null);
const canvas = ref<HTMLCanvasElement | null>(null);
let view: View = { scale: 1, tx: 0, ty: 0 };
// The view follows the card's size and the map's extent until a person
// pans or zooms; from then on it is theirs, until they press Fit.
let autoFit = true;
let grid: PointGrid | null = null;
const dark = window.matchMedia("(prefers-color-scheme: dark)");

const points = computed<MapPoint[]>(() => map.value?.points ?? []);
const legend = computed(() => legendFor(points.value, by.value));
const slotFor = computed(() => slotOf(legend.value));
const categories = computed(() => points.value.map((p) => categoryOf(p, by.value)));

function keyOf(i: number): string {
  const c = categories.value[i];
  return slotFor.value(c) === null ? OTHER : c;
}

function visible(i: number): boolean {
  return !hidden.value.has(keyOf(i));
}

// --- the title, the help, the state -----------------------------------
function retitle() {
  props.ctx.setTitle(applied.value ? `Map: ${applied.value}` : "Embedding map");
}
retitle();
props.ctx.setHelp(`
<p>Every document qmd has embedded, placed so that documents with
similar content sit near each other. The layout is UMAP over qmd's
embeddings, computed by the <code>unified_index/embedding_map</code>
step. Each sync starts from the last layout, so documents already on
the map stay where they were as new ones arrive; <b>Lay out afresh</b>
throws the old layout away.</p>
<p>The search bar takes the grid's grammar (<code>source_id:slack</code>,
<code>after:2025-01-01</code>, free text): what it matches stays in
colour, the rest greys out. Free text asks qmd, so it lights up the
best few hundred matches rather than every mention. <b>Open in grid</b>
lists the same search as a table.</p>
<p>Colour by a field with the picker; hover a legend entry to isolate
it, click it to hide or show it. Drag to pan, scroll to zoom, <b>Fit</b>
to see everything. Hover a point for a preview; click it to open the
document beside this card (with a modifier key, in a new tab).</p>
`);

function saveState() {
  props.ctx.host.setState(encodeState({ q: applied.value, by: by.value, sel: selected.value }));
}

// --- loading -------------------------------------------------------------
let inflight: AbortController | null = null;
async function load() {
  inflight?.abort();
  const abort = new AbortController();
  inflight = abort;
  loading.value = true;
  try {
    const got = await api.fetchEmbeddingMap(abort.signal);
    if (abort.signal.aborted) return;
    map.value = got;
    loadError.value = null;
    grid = PointGrid.over(got.points);
    hovered.value = -1;
    if (autoFit) fit();
    schedule();
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    loadError.value = (e as Error).message;
  } finally {
    if (inflight === abort) loading.value = false;
  }
}

let filterInflight: AbortController | null = null;
async function loadMatches() {
  filterInflight?.abort();
  if (!applied.value.trim()) {
    matches.value = null;
    schedule();
    return;
  }
  const abort = new AbortController();
  filterInflight = abort;
  filtering.value = true;
  try {
    const got = await api.fetchMapMatches(applied.value, abort.signal);
    if (abort.signal.aborted) return;
    matches.value = got;
    schedule();
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    loadError.value = (e as Error).message;
  } finally {
    if (filterInflight === abort) filtering.value = false;
  }
}

function lit(p: MapPoint): boolean {
  return matches.value === null || matches.value.has(p.markdown_uuid);
}

let lastFinished: string | null | undefined;
async function loadStep() {
  try {
    const dag = await api.fetchDag();
    const s = dag.steps.find((x) => x.id === EMBEDDING_MAP_STEP) ?? null;
    step.value = s;
    configured.value = dag.ok ? s !== null : configured.value;
    const finished = s?.last_run?.status === "succeeded" ? s.last_run.finished_at : null;
    // A run that finished since we last looked wrote a new map.
    if (lastFinished !== undefined && finished && finished !== lastFinished) void load();
    lastFinished = finished;
  } catch {
    // The status line is advice; the map itself still loads.
  }
}

let debounce: ReturnType<typeof setTimeout> | null = null;
watch(query, (q) => {
  if (debounce) clearTimeout(debounce);
  debounce = setTimeout(() => apply(q), 350);
});

function apply(q: string) {
  if (debounce) clearTimeout(debounce);
  if (q === applied.value) return;
  applied.value = q;
  retitle();
  saveState();
  void loadMatches();
}

watch(by, () => {
  hidden.value = new Set();
  saveState();
  schedule();
});

// --- the step's own controls --------------------------------------------
const running = computed(() => step.value?.current_state === "running");
const stepMessage = computed(() => {
  const s = step.value;
  if (!s) return null;
  if (s.current_state === "running") return s.progress?.msg ?? "laying out…";
  if (s.current_state === "blocked") {
    return "waiting for the qmd index, which this data root has not built yet — sync a source";
  }
  if (s.last_run?.status === "failed") return `the last layout failed: ${s.last_run.error ?? ""}`;
  return null;
});

async function act(what: string, run: () => Promise<void>) {
  busy.value = what;
  try {
    await run();
  } catch (e) {
    loadError.value = (e as Error).message;
  } finally {
    busy.value = null;
  }
}

function syncMap() {
  return act("syncing", async () => {
    await api.openRequest([EMBEDDING_MAP_STEP]);
    await loadStep();
  });
}

async function layOutAfresh() {
  const sure = await confirmAction(
    "Throw away the current layout and compute a new one? Documents will land in new places.",
  );
  if (!sure) return;
  return act("resetting", async () => {
    await api.resetSteps([EMBEDDING_MAP_STEP]);
    autoFit = true;
    await api.openRequest([EMBEDDING_MAP_STEP]);
    await loadStep();
  });
}

function addStep() {
  return act("adding the step", async () => {
    const cfg = await api.fetchConfig();
    const res = await api.saveConfig(`${cfg.text.trimEnd()}\n${STEP_STANZA}`);
    if (!res.ok) throw new Error(res.error ?? "config.toml refused the step");
    props.ctx.bus.publish(TOPIC_CONFIG_WRITTEN, null);
    await api.openRequest([EMBEDDING_MAP_STEP]);
    await loadStep();
  });
}

function openInGrid() {
  props.ctx.host.openCards(`gridView(${JSON.stringify({ q: applied.value })})`);
}

// --- drawing -------------------------------------------------------------
let frame = 0;
function schedule() {
  if (frame) return;
  frame = requestAnimationFrame(() => {
    frame = 0;
    draw();
  });
}

function css(name: string, fallback: string): string {
  const v = wrap.value ? getComputedStyle(wrap.value).getPropertyValue(name).trim() : "";
  return v || fallback;
}

function draw() {
  const c = canvas.value;
  if (!c) return;
  const g = c.getContext("2d");
  if (!g) return;
  const dpr = window.devicePixelRatio || 1;
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  const w = c.width / dpr;
  const h = c.height / dpr;
  g.clearRect(0, 0, w, h);
  const pts = points.value;
  if (pts.length === 0) return;
  const mode = dark.matches ? "dark" : "light";
  const slots = SLOTS[mode];
  const faded = css("--datalib-border", mode === "dark" ? "#2f3136" : "#d8d8d8");
  const r = Math.max(1.6, Math.min(5, 2.2 * Math.sqrt(view.scale / fitScale)));
  const focus = focusKey.value;

  // One path per colour: the faded first, so what matches sits on top.
  const buckets = new Map<string, number[]>();
  const push = (color: string, i: number) => {
    const b = buckets.get(color);
    if (b) b.push(i);
    else buckets.set(color, [i]);
  };
  for (let i = 0; i < pts.length; i++) {
    if (!visible(i)) continue;
    const key = keyOf(i);
    if (!lit(pts[i]) || (focus !== null && focus !== key)) {
      push(`0:${faded}`, i);
      continue;
    }
    const slot = slotFor.value(categories.value[i]);
    push(`1:${slot === null ? OTHER_COLOR[mode] : slots[slot]}`, i);
  }
  for (const [tag, idx] of [...buckets.entries()].sort()) {
    const faint = tag.startsWith("0");
    // What a filter kept is drawn a size up, so a few matches among
    // thousands still read at a glance.
    const size = !faint && matches.value !== null ? r + 1 : r;
    g.fillStyle = tag.slice(2);
    g.globalAlpha = faint ? 0.7 : 0.85;
    g.beginPath();
    for (const i of idx) {
      const [sx, sy] = toScreen(view, pts[i].x, pts[i].y);
      if (sx < -size || sy < -size || sx > w + size || sy > h + size) continue;
      g.moveTo(sx + size, sy);
      g.arc(sx, sy, size, 0, Math.PI * 2);
    }
    g.fill();
  }
  g.globalAlpha = 1;
  const ring = (i: number, color: string, width: number) => {
    const [sx, sy] = toScreen(view, pts[i].x, pts[i].y);
    g.beginPath();
    g.arc(sx, sy, r + 3, 0, Math.PI * 2);
    g.lineWidth = width;
    g.strokeStyle = color;
    g.stroke();
  };
  const sel = selected.value ? pts.findIndex((p) => p.markdown_uuid === selected.value) : -1;
  if (sel >= 0) ring(sel, css("--datalib-accent", "#2563eb"), 2);
  if (hovered.value >= 0) ring(hovered.value, css("--datalib-fg", "#1a1a1a"), 1.5);
}

let fitScale = 1;
function fit() {
  const c = canvas.value;
  if (!c || points.value.length === 0) return;
  const dpr = window.devicePixelRatio || 1;
  view = fitView(points.value, c.width / dpr, c.height / dpr);
  fitScale = view.scale;
  schedule();
}

function refit() {
  autoFit = true;
  fit();
}

function resize() {
  const c = canvas.value;
  const box = wrap.value;
  if (!c || !box) return;
  const dpr = window.devicePixelRatio || 1;
  const { width, height } = box.getBoundingClientRect();
  c.width = Math.max(1, Math.round(width * dpr));
  c.height = Math.max(1, Math.round(height * dpr));
  c.style.width = `${width}px`;
  c.style.height = `${height}px`;
  if (autoFit) fit();
  schedule();
}

// --- pointer -------------------------------------------------------------
let drag: { x: number; y: number; tx: number; ty: number; moved: boolean } | null = null;

function local(ev: MouseEvent): [number, number] {
  const rect = canvas.value!.getBoundingClientRect();
  return [ev.clientX - rect.left, ev.clientY - rect.top];
}

function onPointerDown(ev: PointerEvent) {
  if (ev.button !== 0) return;
  const [x, y] = local(ev);
  drag = { x, y, tx: view.tx, ty: view.ty, moved: false };
  canvas.value?.setPointerCapture(ev.pointerId);
}

function onPointerMove(ev: PointerEvent) {
  const [x, y] = local(ev);
  if (drag) {
    const dx = x - drag.x;
    const dy = y - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
    if (drag.moved) {
      autoFit = false;
      view = { ...view, tx: drag.tx + dx, ty: drag.ty + dy };
      tip.value = null;
      hovered.value = -1;
      schedule();
      return;
    }
  }
  hover(x, y);
}

function onPointerUp(ev: PointerEvent) {
  const wasClick = drag && !drag.moved;
  drag = null;
  canvas.value?.releasePointerCapture(ev.pointerId);
  if (wasClick && hovered.value >= 0) open(points.value[hovered.value], ev);
}

function onLeave() {
  if (drag) return;
  hovered.value = -1;
  tip.value = null;
  schedule();
}

function onWheel(ev: WheelEvent) {
  ev.preventDefault();
  const [x, y] = local(ev);
  autoFit = false;
  view = zoomAt(view, x, y, Math.exp(-ev.deltaY * 0.0015));
  hover(x, y);
  schedule();
}

function hover(x: number, y: number) {
  if (!grid) return;
  const [dx, dy] = toData(view, x, y);
  // A match under the pointer wins over a greyed-out point nearer it.
  const reach = 8 / view.scale;
  const pts = points.value;
  let i = grid.nearest(dx, dy, reach, (j) => visible(j) && lit(pts[j]));
  if (i < 0) i = grid.nearest(dx, dy, reach, visible);
  if (i !== hovered.value) {
    hovered.value = i;
    schedule();
    if (i >= 0) void loadPreview(points.value[i].markdown_uuid);
  }
  tip.value = i >= 0 ? { x, y } : null;
}

let previewTimer: ReturnType<typeof setTimeout> | null = null;
function loadPreview(uuid: string) {
  if (previewTimer) clearTimeout(previewTimer);
  preview.value = previews.get(uuid) ?? null;
  if (preview.value !== null) return;
  previewTimer = setTimeout(async () => {
    try {
      const chat = await api.fetchChat(uuid);
      previews.set(uuid, previewText(chat.body));
    } catch {
      previews.set(uuid, "");
    }
    const at = hovered.value >= 0 ? points.value[hovered.value] : null;
    if (at?.markdown_uuid === uuid) preview.value = previews.get(uuid) ?? "";
  }, 150);
}

function docSource(p: MapPoint): string {
  return `documentView(${JSON.stringify(p.markdown_uuid)})`;
}

function open(p: MapPoint, ev: MouseEvent) {
  if (isBrowserClick(ev)) {
    window.open(props.ctx.host.hrefFor(docSource(p)), "_blank", "noopener");
    return;
  }
  selected.value = p.markdown_uuid;
  saveState();
  schedule();
  props.ctx.host.openCards(docSource(p));
}

// --- the legend ----------------------------------------------------------
function swatch(slot: number | null): string {
  const mode = dark.matches ? "dark" : "light";
  return slot === null ? OTHER_COLOR[mode] : SLOTS[mode][slot];
}

function toggle(key: string) {
  const next = new Set(hidden.value);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  hidden.value = next;
  schedule();
}

function focus(key: string | null) {
  focusKey.value = key;
  schedule();
}

const tipPoint = computed(() => (hovered.value >= 0 ? points.value[hovered.value] : null));
const tipStyle = computed(() => {
  const t = tip.value;
  const box = wrap.value?.getBoundingClientRect();
  if (!t || !box) return {};
  const left = t.x + 16 + 320 > box.width ? Math.max(4, t.x - 16 - 320) : t.x + 16;
  const top = Math.min(t.y + 12, Math.max(4, box.height - 180));
  return { left: `${left}px`, top: `${top}px` };
});

const summary = computed(() => {
  const m = map.value;
  if (!m?.present) return "";
  const parts = [`${m.points.length.toLocaleString()} documents`];
  if (matches.value !== null) {
    const n = m.points.filter(lit).length;
    parts.push(`${n.toLocaleString()} match`);
  }
  if (m.unembedded) parts.push(`${m.unembedded.toLocaleString()} not embedded yet`);
  if (m.unplaced) parts.push(`${m.unplaced.toLocaleString()} gone from the index`);
  if (m.made_at) parts.push(`laid out ${new Date(m.made_at).toLocaleString()}`);
  return parts.join(" · ");
});

// --- lifecycle -----------------------------------------------------------
let observer: ResizeObserver | null = null;
let unsubscribeLive: (() => void) | null = null;
let unsubscribeBus: (() => void) | null = null;
const onScheme = () => schedule();
onMounted(() => {
  observer = new ResizeObserver(resize);
  if (wrap.value) observer.observe(wrap.value);
  resize();
  dark.addEventListener("change", onScheme);
  void load();
  void loadMatches();
  void loadStep();
  unsubscribeLive = subscribeLive(
    {
      root: (e) => {
        if (changed(e, "dag")) void loadStep();
        // The grid moved: titles, and which documents exist, may have.
        if (e.kind === "index_changed") {
          void load();
          void loadMatches();
        }
      },
      resync: () => {
        void loadStep();
        void load();
        void loadMatches();
      },
    },
    { onScreen: wrap.value ?? undefined },
  );
  unsubscribeBus = props.ctx.bus.subscribe(TOPIC_CONFIG_WRITTEN, () => void loadStep());
});
onBeforeUnmount(() => {
  observer?.disconnect();
  unsubscribeLive?.();
  unsubscribeBus?.();
  dark.removeEventListener("change", onScheme);
  inflight?.abort();
  filterInflight?.abort();
  if (frame) cancelAnimationFrame(frame);
  if (debounce) clearTimeout(debounce);
  if (previewTimer) clearTimeout(previewTimer);
});
</script>

<template>
  <div class="map-card">
    <div class="map-bar">
      <input
        v-model="query"
        class="map-search"
        type="search"
        placeholder="Filter: free text, source_id:…, after:2025-01-01"
        spellcheck="false"
        @keydown.enter="apply(query)"
      />
      <label class="map-by">
        Colour by
        <select v-model="by">
          <option v-for="c in COLOR_BY" :key="c.key" :value="c.key">{{ c.label }}</option>
        </select>
      </label>
      <button type="button" title="Show the whole map" @click="refit">Fit</button>
      <button
        type="button"
        :disabled="!applied"
        title="List what the filter matches as a table"
        @click="openInGrid"
      >
        Open in grid
      </button>
    </div>
    <div class="map-status">
      <span>{{ summary }}</span>
      <span v-if="loading" class="map-muted">loading…</span>
      <span v-if="filtering" class="map-muted">filtering…</span>
      <span v-if="stepMessage" class="map-muted">{{ stepMessage }}</span>
      <span class="map-spacer" />
      <button
        v-if="configured && map?.present"
        type="button"
        class="map-link"
        :disabled="!!busy || running"
        title="Compute the layout again from nothing, instead of from the current one"
        @click="layOutAfresh"
      >
        Lay out afresh
      </button>
    </div>
    <div class="map-body">
      <div ref="wrap" class="map-canvas-wrap">
        <canvas
          ref="canvas"
          :class="{ 'map-pointer': hovered >= 0 }"
          @pointerdown="onPointerDown"
          @pointermove="onPointerMove"
          @pointerup="onPointerUp"
          @pointerleave="onLeave"
          @wheel="onWheel"
        />
        <div v-if="tipPoint && tip" class="map-tip" :style="tipStyle">
          <div class="map-tip-title">{{ tipPoint.title || "(untitled)" }}</div>
          <div class="map-tip-meta">
            {{
              [tipPoint.provider, tipPoint.source, tipPoint.kind, tipPoint.created_at?.slice(0, 10)]
                .filter(Boolean)
                .join(" · ")
            }}
          </div>
          <div v-if="preview" class="map-tip-body">{{ preview }}</div>
          <div v-else-if="preview === null" class="map-muted">loading preview…</div>
        </div>
        <div v-if="map && !map.present && !loadError" class="map-empty">
          <template v-if="configured === false">
            <p>
              There is no map yet, and <code>config.toml</code> has no step to make one. The step
              reads qmd's embeddings, so it needs <code>unified_index/qmd_index</code>.
            </p>
            <pre>{{ STEP_STANZA.trim() }}</pre>
            <button type="button" :disabled="!!busy" @click="addStep">
              {{ busy ?? "Add the step and lay out the map" }}
            </button>
          </template>
          <template v-else-if="running">
            <p>Laying out the map: {{ stepMessage }}</p>
          </template>
          <template v-else>
            <p>The map has not been laid out yet.</p>
            <p v-if="stepMessage" class="map-muted">{{ stepMessage }}</p>
            <button type="button" :disabled="!!busy" @click="syncMap">
              {{ busy ?? "Lay out the map" }}
            </button>
          </template>
        </div>
        <div v-if="loadError" class="map-empty map-error">{{ loadError }}</div>
      </div>
      <ul v-if="legend.length" class="map-legend" @mouseleave="focus(null)">
        <li
          v-for="e in legend"
          :key="e.key"
          :class="{ 'map-off': hidden.has(e.key) }"
          :title="hidden.has(e.key) ? 'Click to show' : 'Click to hide; hover to isolate'"
          @mouseenter="focus(e.key)"
          @click="toggle(e.key)"
        >
          <span class="map-swatch" :style="{ background: swatch(e.slot) }" />
          <span class="map-key">{{ e.key }}</span>
          <span class="map-count">{{ e.count.toLocaleString() }}</span>
        </li>
      </ul>
    </div>
  </div>
</template>

<style>
.map-card {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  font:
    13px/1.4 system-ui,
    -apple-system,
    sans-serif;
  color: var(--datalib-fg, inherit);
  background: var(--datalib-bg, transparent);
}
.map-bar {
  display: flex;
  gap: 8px;
  align-items: center;
  padding: 8px 10px 4px;
  flex-wrap: wrap;
}
.map-search {
  flex: 1 1 220px;
  min-width: 0;
  padding: 4px 8px;
  font: inherit;
  color: inherit;
  background: var(--datalib-input-bg, transparent);
  border: 1px solid var(--datalib-border, #8884);
  border-radius: 4px;
}
.map-by {
  display: flex;
  gap: 4px;
  align-items: center;
  color: var(--datalib-muted, inherit);
}
.map-card select,
.map-card button {
  font: inherit;
  color: inherit;
  background: var(--datalib-input-bg, transparent);
  border: 1px solid var(--datalib-border, #8884);
  border-radius: 4px;
  padding: 3px 8px;
  cursor: pointer;
}
.map-card button:disabled {
  opacity: 0.5;
  cursor: default;
}
.map-card button.map-link {
  border: none;
  background: none;
  color: var(--datalib-accent, inherit);
  padding: 0;
}
.map-status {
  display: flex;
  gap: 10px;
  align-items: baseline;
  padding: 0 10px 6px;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  border-bottom: 1px solid var(--datalib-border, #8884);
}
.map-spacer {
  flex: 1;
}
.map-muted {
  color: var(--datalib-muted, inherit);
}
.map-body {
  flex: 1;
  display: flex;
  min-height: 0;
}
.map-canvas-wrap {
  position: relative;
  flex: 1;
  min-width: 0;
  overflow: hidden;
}
.map-canvas-wrap canvas {
  display: block;
  touch-action: none;
  cursor: grab;
}
.map-canvas-wrap canvas.map-pointer {
  cursor: pointer;
}
.map-tip {
  position: absolute;
  width: 320px;
  pointer-events: none;
  padding: 8px 10px;
  background: var(--datalib-card-bg, #fafafa);
  border: 1px solid var(--datalib-border, #8884);
  border-radius: 6px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.18);
}
.map-tip-title {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.map-tip-meta {
  color: var(--datalib-muted, inherit);
  font-size: 12px;
  margin-bottom: 4px;
}
.map-tip-body {
  font-size: 12px;
  display: -webkit-box;
  -webkit-line-clamp: 6;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.map-empty {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  padding: 24px;
  text-align: center;
}
.map-empty p {
  max-width: 460px;
  margin: 0;
}
.map-empty pre {
  text-align: left;
  font-size: 12px;
  padding: 8px 10px;
  background: var(--datalib-code-bg, #8881);
  border-radius: 4px;
}
.map-error {
  color: var(--datalib-log-error, inherit);
}
.map-legend {
  list-style: none;
  margin: 0;
  padding: 8px 10px;
  width: 200px;
  flex: 0 0 auto;
  overflow-y: auto;
  border-left: 1px solid var(--datalib-border, #8884);
}
.map-legend li {
  display: flex;
  gap: 6px;
  align-items: center;
  padding: 3px 4px;
  border-radius: 4px;
  cursor: pointer;
}
.map-legend li:hover {
  background: var(--datalib-hover, #8882);
}
.map-legend li.map-off {
  opacity: 0.4;
}
.map-swatch {
  flex: 0 0 auto;
  width: 10px;
  height: 10px;
  border-radius: 50%;
}
.map-key {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.map-count {
  color: var(--datalib-muted, inherit);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}
@media (max-width: 520px) {
  .map-legend {
    width: 130px;
  }
}
</style>
