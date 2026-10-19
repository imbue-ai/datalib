<script setup lang="ts">
// One chart of time series: an axis per quantity, two at most, lines with
// a min–max band where the answer was bucketed, and a hover readout. The
// Time series card and a chart inside a document both draw with it; its
// styles are timeSeriesChart.css, which each of them includes.
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import {
  axisTicks,
  bandOf,
  displayUnit,
  extent,
  formatTick,
  formatTime,
  formatTimeTick,
  formatValue,
  lineLabels,
  lineOf,
  nearest,
  quantitiesOf,
  type Answered,
  type Listed,
  type Shown,
  type TimeKind,
} from "./timeSeries";

const props = defineProps<{
  picked: Listed[];
  answered: Map<string, Answered>;
  span: { fromMs: number; toMs: number } | null;
  colorOf: (id: string) => string;
}>();
const emit = defineEmits<{ resize: [width: number] }>();

const chartEl = ref<HTMLElement | null>(null);
const size = ref({ w: 600, h: 320 });

const M = { top: 12, bottom: 28, left: 56 };
const quantities = computed(() => quantitiesOf(props.picked));
const labels = computed(() => lineLabels(props.picked));
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
  quantities.value.slice(0, 2).flatMap((quantity) => {
    const on = props.picked.filter((p) => p.quantity === quantity);
    const values = on.flatMap((p) => {
      const a = props.answered.get(p.time_series_id);
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
  const s = props.span;
  if (!s || s.toMs === s.fromMs) return M.left;
  return M.left + ((ms - s.fromMs) / (s.toMs - s.fromMs)) * plotW.value;
}
function py(v: number, axis: Axis): number {
  return M.top + plotH.value - ((v - axis.lo) / (axis.hi - axis.lo)) * plotH.value;
}

const drawn = computed(() =>
  props.picked.flatMap((p) => {
    const a = props.answered.get(p.time_series_id);
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
        color: props.colorOf(p.time_series_id),
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
  const s = props.span;
  if (!s) return [];
  const n = Math.max(2, Math.min(6, Math.floor(plotW.value / 140)));
  const kind: TimeKind = props.picked.every((p) => p.time_kind === "day") ? "day" : "instant";
  return Array.from({ length: n + 1 }, (_, i) => {
    const ms = s.fromMs + ((s.toMs - s.fromMs) * i) / n;
    return {
      x: px(ms),
      label: formatTimeTick(ms, kind, s.toMs - s.fromMs, i === 0),
      anchor: i === 0 ? "start" : i === n ? "end" : "middle",
    };
  });
});

const hoverMs = ref<number | null>(null);
function onMove(ev: MouseEvent) {
  const s = props.span;
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
  const s = props.span;
  if (at === null || !s) return null;
  const within = (s.toMs - s.fromMs) * 0.02;
  const rows = drawn.value.flatMap((d) => {
    const p = nearest(d.line, at, within);
    return p
      ? [
          {
            id: d.listed.time_series_id,
            color: d.color,
            label: labels.value.get(d.listed.time_series_id) ?? d.listed.human_name,
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
    when: formatTime(x, rows[0]!.kind, s.toMs - s.fromMs),
    rows,
    flip: left > size.value.w / 2,
  };
});

// A chart inside a document lives in the document's frame, so it is
// watched with that window's ResizeObserver.
let observer: ResizeObserver | null = null;
onMounted(() => {
  const el = chartEl.value;
  const Observer = el?.ownerDocument.defaultView?.ResizeObserver ?? globalThis.ResizeObserver;
  if (!el || !Observer) return;
  observer = new Observer(([entry]) => {
    if (!entry) return;
    const { width, height } = entry.contentRect;
    if (width > 0 && height > 0) {
      size.value = { w: width, h: height };
      emit("resize", width);
    }
  });
  observer.observe(el);
});
onBeforeUnmount(() => observer?.disconnect());
</script>

<template>
  <div ref="chartEl" class="ts-chart" @mousemove="onMove" @mouseleave="hoverMs = null">
    <svg
      v-if="picked.length"
      :width="size.w"
      :height="size.h"
      role="img"
      :aria-label="picked.map((p) => labels.get(p.time_series_id)).join(', ')"
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
      :style="hover.flip ? { right: `${size.w - hover.x + 10}px` } : { left: `${hover.x + 10}px` }"
    >
      <div class="ts-when">{{ hover.when }}</div>
      <div v-for="r in hover.rows" :key="r.id">
        <span :style="{ color: r.color }">●</span> {{ r.label }} {{ r.value }}
      </div>
    </div>
  </div>
</template>
