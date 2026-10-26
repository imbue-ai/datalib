<script setup lang="ts">
// One chart of time series: an axis per quantity, two at most, lines with
// a min–max band where the answer was bucketed, a legend that turns lines
// on and off, a hover readout, and zoom. A zoom asks the applet for the
// range again at the plot's width, so it shows finer data rather than
// stretched buckets. The Time series card and a chart inside a document
// both draw with it; its styles are timeSeriesChart.css, and what every
// chart shares is `plot/`.
import { computed, ref, watch } from "vue";
import uPlot from "uplot";
import {
  columnsOf,
  displayUnit,
  axisTicks,
  bandOf,
  extent,
  formatTick,
  formatTime,
  formatTimeTick,
  formatValue,
  lineLabels,
  lineOf,
  nearest,
  quantitiesOf,
  MAX_AXES,
  type Answered,
  type Listed,
  type Shown,
  type TimeKind,
} from "./timeSeries";
import { fetchPoints } from "./timeSeriesApi";
import { axis, withAlpha, type Theme } from "./plot/plot";
import { usePlot, type Range } from "./plot/usePlot";
import PlotLegend from "./plot/PlotLegend.vue";

const props = defineProps<{
  picked: Listed[];
  answered: Map<string, Answered>;
  span: { fromMs: number; toMs: number } | null;
  /// A line's colour, as CSS.
  colorOf: (id: string) => string;
  /// Where given, the legend offers to take a line off the chart.
  remove?: (id: string) => void;
}>();
const emit = defineEmits<{ resize: [width: number] }>();

const box = ref<HTMLElement | null>(null);
const width = ref(600);

const quantities = computed(() => quantitiesOf(props.picked).slice(0, MAX_AXES));
const labels = computed(() => lineLabels(props.picked));

// ── Zoom ───────────────────────────────────────────────────────────

/// What the applet answered for the range zoomed to, or null when not
/// zoomed (or not answered yet), when the card's answer is drawn.
const zoomAnswered = ref<Map<string, Answered> | null>(null);
const zoomError = ref<string | null>(null);
const zoomedTo = ref<Range | null>(null);
let zoomAsked = 0;

async function zoomed(range: Range | null) {
  const mine = ++zoomAsked;
  zoomedTo.value = range;
  zoomError.value = null;
  if (!range) {
    zoomAnswered.value = null;
    return;
  }
  try {
    const u = plot.value;
    const plotWidth = u ? u.bbox.width / uPlot.pxRatio : width.value;
    const got = await fetchPoints(
      props.picked,
      { fromMs: range[0], toMs: range[1] },
      Math.max(50, Math.round(plotWidth)),
    );
    if (mine !== zoomAsked) return;
    zoomAnswered.value = got.answered;
    if (got.unreadable.length) zoomError.value = got.unreadable.map((u) => u.error).join("\n");
  } catch (e) {
    if (mine === zoomAsked) zoomError.value = (e as Error).message;
  }
}

const answers = computed(() => zoomAnswered.value ?? props.answered);
/// The picked lines that have an answer, in pick order: the plot's
/// series, after x.
const drawn = computed(() =>
  props.picked.flatMap((p) => {
    const a = answers.value.get(p.time_series_id);
    const q = quantities.value.indexOf(p.quantity);
    return a && q >= 0 ? [{ listed: p, answer: a, axisIdx: q }] : [];
  }),
);

// ── Axes ───────────────────────────────────────────────────────────

/// One value axis, in SI units: `lo`, `hi` and `ticks` are SI values,
/// and `shown` turns one into what the axis prints.
type Axis = { lo: number; hi: number; shown: Shown; ticks: number[] };

const axes = computed<(Axis | null)[]>(() =>
  quantities.value.map((quantity) => {
    const on = drawn.value.filter((d) => d.listed.quantity === quantity);
    const values = on.flatMap(({ answer }) => [
      ...lineOf(answer).map((pt) => pt.y),
      ...(bandOf(answer) ?? []).flatMap((b) => [b.lo, b.hi]),
    ]);
    const e = extent(values);
    if (!e || !on[0]) return null;
    const shown = displayUnit(quantity, on[0].listed, Math.max(Math.abs(e[0]), Math.abs(e[1])));
    return { lo: e[0], hi: e[1], shown, ticks: axisTicks(e[0], e[1], shown) };
  }),
);

const baseRange = (): Range => {
  const s = props.span;
  return s && s.toMs > s.fromMs ? [s.fromMs, s.toMs] : [0, 1];
};
const timeKind = computed<TimeKind>(() =>
  props.picked.every((p) => p.time_kind === "day") ? "day" : "instant",
);

// ── Legend ─────────────────────────────────────────────────────────

/// The lines turned off, by id, so a pick added or taken off leaves the
/// others as they were.
const hidden = ref(new Set<string>());
const legend = computed(() =>
  props.picked.map((p) => ({
    key: p.time_series_id,
    label: labels.value.get(p.time_series_id) ?? p.human_name,
    color: props.colorOf(p.time_series_id),
    note:
      quantities.value.length > 1
        ? quantities.value.indexOf(p.quantity) === 0
          ? "left"
          : "right"
        : undefined,
  })),
);
const legendShown = computed(() => props.picked.map((p) => !hidden.value.has(p.time_series_id)));
function setShown(shown: boolean[]) {
  hidden.value = new Set(props.picked.filter((_, i) => !shown[i]).map((p) => p.time_series_id));
}

// ── The plot ───────────────────────────────────────────────────────

/// The plot's margins in CSS pixels. `right` holds half the last time
/// tick, which is centred on the plot's right edge.
const M = { top: 18, side: 56, bottom: 28, right: 36 };
const BAND_ALPHA = 0.14;

/// The time under the pointer, in ms, or null.
const hoverMs = ref<number | null>(null);
/// Where the plot area starts in the chart, in CSS pixels.
let plotLeft = M.side;

/// The series after x: a line per drawn time series, then each one's
/// band as its high and low edges.
const seriesShown = computed(() => {
  const on = drawn.value.map((d) => !hidden.value.has(d.listed.time_series_id));
  return [...on, ...on.flatMap((s) => [s, s])];
});

function data(): uPlot.AlignedData {
  const c = columnsOf(drawn.value.map((d) => d.answer));
  return [c.xs, ...c.lines, ...c.bands.flatMap((b) => [b.hi, b.lo])];
}

function options(theme: Theme) {
  const n = drawn.value.length;
  const scaleOf = (i: number) => `y${drawn.value[i]!.axisIdx}`;
  const colors = drawn.value.map((d) => theme.color(props.colorOf(d.listed.time_series_id)));
  const valueAxis = (i: number, side: 1 | 3): uPlot.Axis =>
    axis(theme, "value", {
      scale: `y${i}`,
      side,
      size: M.side,
      grid: i === 0 ? { stroke: theme.grid, width: 1 } : { show: false },
      splits: () => axes.value[i]?.ticks ?? [],
      values: (_u, splits) => splits.map((t) => formatTick(t, axes.value[i]!.shown)),
    });
  return {
    padding: [M.top, quantities.value.length > 1 ? 0 : M.right, 0, 0] as uPlot.Padding,
    legend: { show: false },
    cursor: { points: { show: false } },
    scales: Object.fromEntries([
      ["x", { time: false }],
      ...quantities.value.map((_, i) => [
        `y${i}`,
        { range: (): Range => (axes.value[i] ? [axes.value[i]!.lo, axes.value[i]!.hi] : [0, 1]) },
      ]),
    ]),
    axes: [
      axis(theme, "time", {
        size: M.bottom,
        splits: (u: uPlot, _axis: number, min: number, max: number) => {
          const k = Math.max(2, Math.min(6, Math.floor(u.bbox.width / uPlot.pxRatio / 140)));
          return Array.from({ length: k + 1 }, (_, i) => min + ((max - min) * i) / k);
        },
        values: (u: uPlot, splits: number[]) => {
          const spanMs = (u.scales.x.max ?? 1) - (u.scales.x.min ?? 0);
          return splits.map((ms, i) => formatTimeTick(ms, timeKind.value, spanMs, i === 0));
        },
      }),
      ...quantities.value.map((_, i) => valueAxis(i, i === 0 ? 3 : 1)),
    ],
    series: [
      {},
      ...drawn.value.map((d, i) => ({
        label: labels.value.get(d.listed.time_series_id),
        scale: scaleOf(i),
        stroke: colors[i],
        width: 2,
        spanGaps: true,
        points: { show: false },
      })),
      ...drawn.value.flatMap((_, i) =>
        ["hi", "lo"].map((edge) => ({
          label: edge,
          scale: scaleOf(i),
          stroke: "transparent",
          width: 0,
          spanGaps: true,
          points: { show: false },
        })),
      ),
    ],
    bands: drawn.value.map((_, i) => ({
      series: [n + 1 + 2 * i, n + 2 + 2 * i] as uPlot.Band.Bounds,
      fill: withAlpha(colors[i]!, BAND_ALPHA),
    })),
    hooks: {
      setCursor: [
        (u: uPlot) => {
          const left = u.cursor.left;
          hoverMs.value = left == null || left < 0 ? null : u.posToVal(left, "x");
          plotLeft = u.bbox.left / uPlot.pxRatio;
        },
      ],
    },
  };
}

const { plot, resetZoom } = usePlot(box, {
  shape: () =>
    JSON.stringify([
      drawn.value.map((d) => [d.listed.time_series_id, props.colorOf(d.listed.time_series_id)]),
      quantities.value,
    ]),
  data,
  options,
  shown: () => seriesShown.value,
  zoom: { base: baseRange, changed: (r) => void zoomed(r) },
  resized: (w) => {
    width.value = w;
    emit("resize", w);
  },
});

// What is drawn changed under the zoom: the card's answer is the one to
// show again.
watch(
  () => JSON.stringify([props.picked.map((p) => p.time_series_id), props.span]),
  () => {
    if (zoomedTo.value === null) return;
    void zoomed(null);
    resetZoom();
  },
);

// ── Hover ──────────────────────────────────────────────────────────

const hover = computed(() => {
  const at = hoverMs.value;
  const u = plot.value;
  if (at === null || !u) return null;
  const [lo, hi] = zoomedTo.value ?? baseRange();
  const within = (hi - lo) * 0.02;
  const rows = drawn.value.flatMap((d) => {
    if (hidden.value.has(d.listed.time_series_id)) return [];
    const p = nearest(lineOf(d.answer), at, within);
    const ax = axes.value[d.axisIdx];
    return p && ax
      ? [
          {
            id: d.listed.time_series_id,
            color: props.colorOf(d.listed.time_series_id),
            label: labels.value.get(d.listed.time_series_id) ?? d.listed.human_name,
            value: formatValue(p.y, ax.shown),
            x: p.x,
            kind: d.listed.time_kind,
          },
        ]
      : [];
  });
  if (!rows.length) return null;
  const x = rows[0]!.x;
  const left = plotLeft + u.valToPos(x, "x");
  return {
    x: left,
    when: formatTime(x, rows[0]!.kind, hi - lo),
    rows,
    flip: left > width.value / 2,
  };
});
</script>

<template>
  <div class="ts-chart-wrap">
    <PlotLegend
      v-if="picked.length"
      class="ts-chart-legend"
      :entries="legend"
      :shown="legendShown"
      @update:shown="setShown"
    >
      <template v-if="remove" #after="{ entry }">
        <button
          type="button"
          class="plot-legend-x"
          :aria-label="`Take ${entry.label} off the chart`"
          @click="remove(entry.key)"
        >
          ×
        </button>
      </template>
    </PlotLegend>
    <div class="ts-chart" :class="{ 'is-empty': !picked.length }">
      <div
        ref="box"
        class="ts-plot"
        role="img"
        :aria-label="picked.map((p) => labels.get(p.time_series_id)).join(', ')"
      />
      <span v-if="axes[0]" class="ts-unit">{{ axes[0].shown.label }}</span>
      <span v-if="axes[1]" class="ts-unit ts-unit-right">{{ axes[1].shown.label }}</span>
      <div
        v-if="hover"
        class="ts-tooltip"
        :style="hover.flip ? { right: `${width - hover.x + 10}px` } : { left: `${hover.x + 10}px` }"
      >
        <div class="ts-when">{{ hover.when }}</div>
        <div v-for="r in hover.rows" :key="r.id">
          <span :style="{ color: r.color }">●</span> {{ r.label }} {{ r.value }}
        </div>
      </div>
    </div>
    <div v-if="zoomError" class="ts-zoom-note">{{ zoomError }}</div>
  </div>
</template>
