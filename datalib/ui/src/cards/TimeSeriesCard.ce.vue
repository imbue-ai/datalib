<script setup lang="ts">
// `timeSeriesView()`: every time series in the time-series index, picked
// from a list and drawn together on one chart, one axis per quantity and
// two axes at most. Read from the `unified_time_series_index` applet; the
// rules are timeSeries.ts.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { subscribeLive } from "@/live";
import {
  RANGES,
  TIME_SERIES_APPLET,
  axisTicks,
  bandOf,
  decodeState,
  displayUnit,
  encodeState,
  extent,
  formatTick,
  formatTime,
  formatValue,
  fromMs,
  grouped,
  lineOf,
  nearest,
  newestMs,
  quantitiesOf,
  rangeBounds,
  refusal,
  toMs,
  unitText,
  type Answered,
  type Listed,
  type RangeKey,
  type Shown,
  type TimeKind,
} from "./timeSeries";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
props.ctx.setTitle("Time series");
props.ctx.setHelp(`
<p>Every time series your sources keep — a Garmin metric by day, a
sensor's readings — from the time-series index. Tick time series in the
list to draw them on one chart. Time series of one quantity share an
axis, converted to one unit — minutes and hours, kilograms and pounds; a
second quantity gets the right-hand axis, and a chart holds two at
most. A time series of text is listed but not drawn yet.</p>
<p>Past a few hundred points a line shows each stretch's mean, with the
shaded band from its lowest to its highest value; a shorter range shows
every point. Hover the chart to read each line's value at that time.</p>
`);

const COLORS = ["#2f6fd0", "#d9661a", "#7c4dcc", "#1f9a6b", "#c2387a", "#8a6d00"];

const restored = decodeState(props.ctx.initialState);
const all = ref<Listed[]>([]);
const ids = ref<string[]>(restored.ids);
const range = ref<RangeKey>(restored.range);
const filter = ref("");
const answered = ref<Map<string, Answered>>(new Map());
const error = ref<string | null>(null);
const notice = ref<string | null>(null);
const loaded = ref(false);

const cardEl = ref<HTMLElement | null>(null);
const chartEl = ref<HTMLElement | null>(null);
const size = ref({ w: 600, h: 320 });

const byId = computed(() => new Map(all.value.map((s) => [s.time_series_id, s])));
const picked = computed(() =>
  ids.value.flatMap((id) => {
    const s = byId.value.get(id);
    return s ? [s] : [];
  }),
);
const groups = computed(() => grouped(all.value, filter.value));
const colorOf = (id: string) => COLORS[Math.max(0, ids.value.indexOf(id)) % COLORS.length]!;

function persist() {
  props.ctx.host.setState(encodeState(ids.value, range.value));
}

function toggle(s: Listed) {
  notice.value = null;
  if (ids.value.includes(s.time_series_id)) {
    ids.value = ids.value.filter((id) => id !== s.time_series_id);
  } else {
    const why = refusal(picked.value, s);
    if (why) {
      notice.value = why;
      return;
    }
    ids.value = [...ids.value, s.time_series_id];
  }
  persist();
}

function pickRange(key: RangeKey) {
  range.value = key;
  persist();
}

// ── Loading ────────────────────────────────────────────────────────

let listAsked = 0;
async function loadList() {
  const mine = ++listAsked;
  try {
    const r = await fetch(`${TIME_SERIES_APPLET}/time_series`);
    const text = await r.text();
    if (!r.ok) throw new Error(`time series → ${r.status}: ${text}`);
    if (mine !== listAsked) return;
    all.value = (JSON.parse(text) as { time_series: Listed[] }).time_series;
    error.value = null;
  } catch (e) {
    if (mine === listAsked) error.value = (e as Error).message;
  } finally {
    if (mine === listAsked) loaded.value = true;
  }
}

/// What the chart spans, in milliseconds: the range ending at the newest
/// point picked, else everything the picked time series hold.
const span = computed<{ fromMs: number; toMs: number } | null>(() => {
  const newest = newestMs(picked.value);
  if (newest === null) return null;
  const bounded = rangeBounds(range.value, newest);
  if (bounded) return bounded;
  const firsts = picked.value.flatMap((p) =>
    p.first_t === null ? [] : [toMs(p.time_kind, p.first_t)],
  );
  return { fromMs: Math.min(...firsts), toMs: newest };
});

let pointsAsked = 0;
async function loadPoints() {
  const mine = ++pointsAsked;
  const s = span.value;
  if (!s || !picked.value.length) {
    answered.value = new Map();
    return;
  }
  // The applet reads `from` and `to` in each time series' own `t`, so a
  // day and an instant are asked for separately.
  const kinds = new Map<TimeKind, string[]>();
  for (const p of picked.value)
    kinds.set(p.time_kind, [...(kinds.get(p.time_kind) ?? []), p.time_series_id]);
  const budget = Math.max(50, Math.round(size.value.w));
  try {
    const answers = await Promise.all(
      [...kinds.entries()].map(async ([kind, kindIds]) => {
        const r = await fetch(`${TIME_SERIES_APPLET}/points`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            ids: kindIds,
            from: fromMs(kind, s.fromMs),
            to: fromMs(kind, s.toMs),
            budget,
          }),
        });
        const text = await r.text();
        if (!r.ok) throw new Error(`points → ${r.status}: ${text}`);
        return JSON.parse(text) as {
          time_series: Answered[];
          unreadable: { time_series_id: string; error: string }[];
        };
      }),
    );
    if (mine !== pointsAsked) return;
    answered.value = new Map(
      answers.flatMap((a) => a.time_series).map((a) => [a.time_series_id, a]),
    );
    const unreadable = answers.flatMap((a) => a.unreadable ?? []);
    error.value = unreadable.length
      ? unreadable
          .map(
            (u) =>
              `${byId.value.get(u.time_series_id)?.human_name ?? u.time_series_id}: ${u.error}`,
          )
          .join("\n")
      : null;
  } catch (e) {
    if (mine === pointsAsked) error.value = (e as Error).message;
  }
}

let pointsTimer: ReturnType<typeof setTimeout> | null = null;
function schedulePoints() {
  if (pointsTimer) clearTimeout(pointsTimer);
  pointsTimer = setTimeout(() => void loadPoints(), 120);
}
watch([ids, range, () => Math.round(size.value.w / 50), all], schedulePoints);

// ── Geometry ───────────────────────────────────────────────────────

const M = { top: 12, bottom: 28, left: 56 };
const quantities = computed(() => quantitiesOf(picked.value));
const marginRight = computed(() => (quantities.value.length > 1 ? 56 : 16));
const plotW = computed(() => Math.max(10, size.value.w - M.left - marginRight.value));
const plotH = computed(() => Math.max(10, size.value.h - M.top - M.bottom));

/// One value axis, in SI units: `lo`, `hi` and `ticks` are SI values,
/// and `shown` turns one into what the axis prints.
type Axis = {
  quantity: string;
  lo: number;
  hi: number;
  shown: Shown;
  ticks: number[];
};

const axes = computed<Axis[]>(() =>
  quantities.value.flatMap((quantity) => {
    const on = picked.value.filter((p) => p.quantity === quantity);
    const values = on.flatMap((p) => {
      const a = answered.value.get(p.time_series_id);
      if (!a) return [];
      const band = bandOf(a) ?? [];
      return [...lineOf(a).map((pt) => pt.y), ...band.flatMap((b) => [b.lo, b.hi])];
    });
    const e = extent(values);
    if (!e) return [];
    const shown = displayUnit(quantity, on[0]!, Math.max(Math.abs(e[0]), Math.abs(e[1])));
    return [{ quantity, lo: e[0], hi: e[1], shown, ticks: axisTicks(e[0], e[1], shown) }];
  }),
);

const axisOf = (quantity: string) => axes.value.find((a) => a.quantity === quantity) ?? null;

function px(ms: number): number {
  const s = span.value;
  if (!s || s.toMs === s.fromMs) return M.left;
  return M.left + ((ms - s.fromMs) / (s.toMs - s.fromMs)) * plotW.value;
}
function py(v: number, axis: Axis): number {
  return M.top + plotH.value - ((v - axis.lo) / (axis.hi - axis.lo)) * plotH.value;
}

const drawn = computed(() =>
  picked.value.flatMap((p) => {
    const a = answered.value.get(p.time_series_id);
    const axis = axisOf(p.quantity);
    if (!a || !axis) return [];
    const line = lineOf(a);
    const band = bandOf(a);
    const points = line
      .map((pt) => `${px(pt.x).toFixed(1)},${py(pt.y, axis).toFixed(1)}`)
      .join(" ");
    const bandPath =
      band && band.length > 1
        ? "M" +
          band.map((b) => `${px(b.x).toFixed(1)},${py(b.hi, axis).toFixed(1)}`).join(" L") +
          " L" +
          [...band]
            .reverse()
            .map((b) => `${px(b.x).toFixed(1)},${py(b.lo, axis).toFixed(1)}`)
            .join(" L") +
          " Z"
        : null;
    return [
      {
        id: p.time_series_id,
        color: colorOf(p.time_series_id),
        points,
        bandPath,
        line,
        axis,
        listed: p,
      },
    ];
  }),
);

const xTicks = computed(() => {
  const s = span.value;
  if (!s) return [];
  const n = Math.max(2, Math.min(6, Math.floor(plotW.value / 140)));
  const kind: TimeKind = picked.value.every((p) => p.time_kind === "day") ? "day" : "instant";
  return Array.from({ length: n + 1 }, (_, i) => {
    const ms = s.fromMs + ((s.toMs - s.fromMs) * i) / n;
    return {
      x: px(ms),
      label: formatTime(ms, kind, s.toMs - s.fromMs),
      anchor: i === 0 ? "start" : i === n ? "end" : "middle",
    };
  });
});

// ── Hover ──────────────────────────────────────────────────────────

const hoverMs = ref<number | null>(null);
function onMove(ev: MouseEvent) {
  const s = span.value;
  const box = chartEl.value?.getBoundingClientRect();
  if (!s || !box) return;
  const x = ev.clientX - box.left - M.left;
  if (x < 0 || x > plotW.value) {
    hoverMs.value = null;
    return;
  }
  hoverMs.value = s.fromMs + (x / plotW.value) * (s.toMs - s.fromMs);
}
const hover = computed(() => {
  const at = hoverMs.value;
  if (at === null || !span.value) return null;
  const within = (span.value.toMs - span.value.fromMs) * 0.02;
  const rows = drawn.value.flatMap((d) => {
    const p = nearest(d.line, at, within);
    return p
      ? [
          {
            color: d.color,
            label: d.listed.human_name,
            value: formatValue(p.y, d.axis.shown),
            x: p.x,
            kind: d.listed.time_kind,
          },
        ]
      : [];
  });
  if (!rows.length) return null;
  const x = rows[0]!.x;
  const left = px(x);
  return {
    x: left,
    when: formatTime(x, rows[0]!.kind, span.value.toMs - span.value.fromMs),
    rows,
    flip: left > size.value.w / 2,
  };
});

// ── Lifecycle ──────────────────────────────────────────────────────

let observer: ResizeObserver | null = null;
let stopLive: (() => void) | null = null;
onMounted(() => {
  void loadList();
  if (chartEl.value && typeof ResizeObserver !== "undefined") {
    observer = new ResizeObserver(([entry]) => {
      if (!entry) return;
      const { width, height } = entry.contentRect;
      if (width > 0 && height > 0) size.value = { w: width, h: height };
    });
    observer.observe(chartEl.value);
  }
  stopLive = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "time_series_index_changed") void loadList();
      },
      resync: () => void loadList(),
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  observer?.disconnect();
  stopLive?.();
  if (pointsTimer) clearTimeout(pointsTimer);
});
</script>

<template>
  <div ref="cardEl" class="ts-card">
    <div class="ts-list">
      <div class="ts-filter">
        <input
          v-model="filter"
          class="ts-input"
          type="search"
          placeholder="Find a time series"
          aria-label="Find a time series"
        />
      </div>
      <div class="ts-groups">
        <div v-if="loaded && !all.length" class="ts-empty">
          No time series yet. A Garmin source writes them on its next sync.
        </div>
        <div v-else-if="loaded && !groups.length" class="ts-empty">
          Nothing matches “{{ filter }}”.
        </div>
        <section v-for="g in groups" :key="g.source" class="ts-group">
          <h3 class="ts-source">{{ g.source }} · {{ g.items.length }}</h3>
          <label
            v-for="s in g.items"
            :key="s.time_series_id"
            class="ts-item"
            :class="{ 'ts-picked': ids.includes(s.time_series_id) }"
          >
            <input type="checkbox" :checked="ids.includes(s.time_series_id)" @change="toggle(s)" />
            <span
              class="ts-swatch"
              :style="{
                background: ids.includes(s.time_series_id)
                  ? colorOf(s.time_series_id)
                  : 'transparent',
              }"
            ></span>
            <span class="ts-label">{{ s.human_name }}</span>
            <span class="ts-meta">{{ unitText(s) }} · {{ s.point_count }}</span>
          </label>
        </section>
      </div>
    </div>
    <div class="ts-main">
      <div class="ts-bar">
        <div class="ts-legend">
          <span v-if="!picked.length" class="ts-hint">Tick a time series to draw it.</span>
          <span
            v-for="p in picked"
            :key="p.time_series_id"
            class="ts-chip"
            :style="{ borderColor: colorOf(p.time_series_id) }"
          >
            <span class="ts-swatch" :style="{ background: colorOf(p.time_series_id) }"></span>
            {{ p.human_name }} · {{ p.source_id }}
            <span class="ts-axis-name">{{
              quantities.length > 1 ? (quantities.indexOf(p.quantity) === 0 ? "left" : "right") : ""
            }}</span>
            <button
              type="button"
              class="ts-x"
              :aria-label="`Take ${p.human_name} off the chart`"
              @click="toggle(p)"
            >
              ×
            </button>
          </span>
        </div>
        <div role="group" aria-label="Range" class="ts-ranges">
          <button
            v-for="r in RANGES"
            :key="r.key"
            type="button"
            class="ts-range"
            :aria-pressed="range === r.key"
            @click="pickRange(r.key)"
          >
            {{ r.label }}
          </button>
        </div>
      </div>
      <div v-if="notice" class="ts-notice">{{ notice }}</div>
      <div v-if="error" class="ts-error">{{ error }}</div>
      <div ref="chartEl" class="ts-chart" @mousemove="onMove" @mouseleave="hoverMs = null">
        <svg
          v-if="picked.length"
          :width="size.w"
          :height="size.h"
          role="img"
          :aria-label="picked.map((p) => p.human_name).join(', ')"
        >
          <g v-if="axes[0]">
            <g v-for="t in axes[0].ticks" :key="`l${t}`">
              <line
                class="ts-grid"
                :x1="M.left"
                :x2="M.left + plotW"
                :y1="py(t, axes[0])"
                :y2="py(t, axes[0])"
              />
              <text class="ts-tick" :x="M.left - 6" :y="py(t, axes[0]) + 4" text-anchor="end">
                {{ formatTick(t, axes[0].shown) }}
              </text>
            </g>
            <text class="ts-tick ts-unit" :x="4" :y="M.top + 10">{{ axes[0].shown.label }}</text>
          </g>
          <g v-if="axes[1]">
            <text
              v-for="t in axes[1].ticks"
              :key="`r${t}`"
              class="ts-tick"
              :x="M.left + plotW + 6"
              :y="py(t, axes[1]) + 4"
            >
              {{ formatTick(t, axes[1].shown) }}
            </text>
            <text class="ts-tick ts-unit" :x="size.w - 4" :y="M.top + 10" text-anchor="end">
              {{ axes[1].shown.label }}
            </text>
          </g>
          <text
            v-for="t in xTicks"
            :key="t.label + t.x"
            class="ts-tick"
            :x="t.x"
            :y="size.h - 8"
            :text-anchor="t.anchor"
          >
            {{ t.label }}
          </text>
          <g v-for="d in drawn" :key="d.id">
            <path v-if="d.bandPath" :d="d.bandPath" :fill="d.color" fill-opacity="0.14" />
            <polyline :points="d.points" fill="none" :stroke="d.color" stroke-width="2" />
          </g>
          <line
            v-if="hover"
            class="ts-cursor"
            :x1="hover.x"
            :x2="hover.x"
            :y1="M.top"
            :y2="M.top + plotH"
          />
        </svg>
        <div
          v-if="hover"
          class="ts-tooltip"
          :style="
            hover.flip ? { right: `${size.w - hover.x + 10}px` } : { left: `${hover.x + 10}px` }
          "
        >
          <div class="ts-when">{{ hover.when }}</div>
          <div v-for="r in hover.rows" :key="r.label">
            <span :style="{ color: r.color }">●</span> {{ r.label }} {{ r.value }}
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style>
.ts-card {
  position: absolute;
  inset: 0;
  display: flex;
  container-type: inline-size;
  color: var(--datalib-fg);
  background: var(--datalib-bg);
  font-size: var(--datalib-font-size);
}
.ts-list {
  flex: 0 0 15rem;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--datalib-border-soft);
  min-height: 0;
  max-height: 100%;
}
.ts-filter {
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.ts-input {
  width: 100%;
  box-sizing: border-box;
  font: inherit;
  padding: 0.2rem 0.45rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.ts-groups {
  flex: 1 1 auto;
  overflow: auto;
  padding-bottom: 0.5rem;
}
.ts-source {
  margin: 0;
  padding: 0.6rem 0.75rem 0.2rem;
  font-size: var(--datalib-font-size-small);
  font-weight: 600;
  color: var(--datalib-muted);
}
.ts-item {
  display: flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.25rem 0.75rem;
  cursor: pointer;
}
.ts-item:hover {
  background: var(--datalib-hover);
}
.ts-picked {
  background: var(--datalib-selected);
}
.ts-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ts-meta {
  white-space: nowrap;
}
.ts-meta,
.ts-hint,
.ts-empty,
.ts-axis-name {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.ts-empty {
  padding: 0.75rem;
}
.ts-swatch {
  display: inline-block;
  width: 0.7rem;
  height: 0.2rem;
  border-radius: 1px;
  flex: none;
}
.ts-main {
  flex: 1 1 0;
  min-width: 0;
  display: flex;
  flex-direction: column;
  padding: 0.5rem 0.75rem;
  gap: 0.4rem;
  min-height: 16rem;
}
.ts-bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.4rem 0.75rem;
}
.ts-legend {
  flex: 1 1 auto;
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem;
}
.ts-chip {
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  padding: 0.1rem 0.25rem 0.1rem 0.55rem;
  border: 1px solid;
  border-radius: 9999px;
  font-size: var(--datalib-font-size-small);
}
.ts-x {
  font: inherit;
  border: 0;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
  padding: 0 0.3rem;
}
.ts-ranges {
  display: flex;
  gap: 0.25rem;
}
.ts-range {
  font: inherit;
  font-size: var(--datalib-font-size-small);
  padding: 0.15rem 0.55rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
}
.ts-range[aria-pressed="true"] {
  background: var(--datalib-accent);
  border-color: var(--datalib-accent);
  color: var(--datalib-on-accent);
}
.ts-notice {
  padding: 0.3rem 0.5rem;
  border: 1px solid var(--datalib-warn-border);
  background: var(--datalib-warn-bg);
  color: var(--datalib-warn-fg);
  border-radius: var(--datalib-radius);
}
.ts-error {
  color: var(--datalib-error-fg);
  white-space: pre-line;
}
/* A narrow card stacks the list over the chart. */
@container (max-width: 34rem) {
  .ts-card {
    flex-direction: column;
  }
  .ts-list {
    flex: 0 0 40%;
    border-right: 0;
    border-bottom: 1px solid var(--datalib-border-soft);
  }
}
.ts-chart {
  position: relative;
  flex: 1 1 auto;
  min-height: 12rem;
  overflow: hidden;
}
.ts-chart > svg {
  position: absolute;
  inset: 0;
}
.ts-grid {
  stroke: var(--datalib-border-soft);
}
.ts-tick {
  fill: var(--datalib-muted);
  font-size: 11px;
}
.ts-unit {
  font-weight: 600;
}
.ts-cursor {
  stroke: var(--datalib-muted);
  stroke-dasharray: 3 3;
}
.ts-tooltip {
  position: absolute;
  top: 0.5rem;
  pointer-events: none;
  padding: 0.35rem 0.55rem;
  background: var(--datalib-card-bg);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  box-shadow: var(--datalib-shadow);
  font-size: var(--datalib-font-size-small);
  line-height: 1.5;
  white-space: nowrap;
}
.ts-when {
  font-weight: 600;
}
</style>
