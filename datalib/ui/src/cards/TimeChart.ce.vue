<script setup lang="ts">
// One small time chart on the sync dashboard: a step function per line
// over the run. Every chart on the card shares the time axis and,
// through uPlot's cursor sync, one crosshair and one zoom. The header
// reads the value under the crosshair, or the latest when there is none;
// a legend names each line, with its value, when there is more than
// one. Which charts, and the arithmetic, are `dashboardCharts.ts`; what
// every chart shares is `plot/`.
import { computed, ref, watch } from "vue";
import uPlot from "uplot";
import { formatBytes } from "@/config/bytes";
import { formatClock } from "@/config/timeFormat";
import { aligned, valueAt, yRange, type Chart, type Line } from "./dashboardCharts";
import { axis, seriesColor } from "./plot/plot";
import { usePlot } from "./plot/usePlot";
import PlotLegend from "./plot/PlotLegend.vue";

const props = defineProps<{
  chart: Chart;
  /// The span every chart on the card covers, in ms.
  domain: [number, number];
  /// Where the lines stop being carried forward: the step's finish, or now.
  end: number;
  /// Charts with the same key share one crosshair and one zoom.
  syncKey: string;
}>();

const HEIGHT = 96;

const box = ref<HTMLElement | null>(null);
/// The instant under the crosshair, in ms, or null.
const cursorAt = ref<number | null>(null);

const format = (v: number | null) =>
  v === null ? "—" : props.chart.unit === "bytes" ? formatBytes(v) : v.toLocaleString();

/// What the header and legend read: under the crosshair, or the latest.
const readAt = computed(() =>
  cursorAt.value === null ? props.end : Math.min(cursorAt.value, props.end),
);
const valueOf = (l: Line) => valueAt(l.points, readAt.value);

const headline = computed(() => {
  const [first] = props.chart.lines;
  if (props.chart.lines.length !== 1 || !first) return null;
  return format(valueOf(first));
});

/// A line's colour, as CSS.
function colorOf(l: Line): string {
  const c = l.color;
  if ("slot" in c) return seriesColor(c.slot);
  if ("status" in c)
    return `var(${c.status === "warn" ? "--datalib-log-warn" : "--datalib-log-error"})`;
  return "var(--datalib-accent)";
}

const legend = computed(() =>
  props.chart.lines.map((l) => ({
    key: l.label,
    label: l.label,
    color: colorOf(l),
    value: format(valueOf(l)),
  })),
);

/// The shape a plot is built for: another line, a renamed one or another
/// unit needs a new plot; anything else is new data on the old one.
const shape = computed(() =>
  JSON.stringify([props.chart.unit, props.chart.lines.map((l) => [l.label, l.color])]),
);
const shown = ref<boolean[]>([]);
watch(shape, () => (shown.value = props.chart.lines.map(() => true)), { immediate: true });

/// The last drawn point of each line, ringed: a line with one sample is
/// otherwise a line of no length, and the dot says where it stands now.
function drawEnds(u: uPlot, ring: string) {
  const ctx = u.ctx;
  const xs = u.data[0];
  u.series.forEach((s, i) => {
    if (i === 0 || !s.show) return;
    const ys = u.data[i]!;
    let last = ys.length - 1;
    while (last >= 0 && ys[last] == null) last--;
    if (last < 0) return;
    const x = u.valToPos(xs[last]!, "x", true);
    const y = u.valToPos(ys[last] as number, "y", true);
    ctx.beginPath();
    ctx.arc(x, y, 3 * uPlot.pxRatio, 0, 2 * Math.PI);
    ctx.fillStyle = s.stroke as unknown as string;
    ctx.fill();
    ctx.lineWidth = 1.5 * uPlot.pxRatio;
    ctx.strokeStyle = ring;
    ctx.stroke();
  });
}

const domainS = (): [number, number] => [props.domain[0] / 1000, props.domain[1] / 1000];

const { resetZoom } = usePlot(box, {
  height: HEIGHT,
  syncKey: props.syncKey,
  shape: () => shape.value,
  shown: () => shown.value,
  zoom: { base: domainS },
  data: () => {
    const { xs, ys } = aligned(props.chart, props.domain, props.end);
    return [xs, ...ys];
  },
  options: (theme) => ({
    legend: { show: false },
    cursor: { points: { show: false } },
    scales: {
      x: { time: true },
      y: { range: () => yRange(props.chart, props.domain) },
    },
    axes: [
      axis(theme, "time", {
        font: theme.fontSmall,
        size: 20,
        space: 72,
        values: (_u, splits) => splits.map((s) => formatClock(s * 1000)),
      }),
      axis(theme, "value", {
        font: theme.fontSmall,
        size: 46,
        space: 22,
        values: (_u, splits) => splits.map((v) => format(v)),
      }),
    ],
    series: [
      {},
      ...props.chart.lines.map((l) => ({
        label: l.label,
        stroke: theme.color(colorOf(l)),
        width: 2,
        paths: uPlot.paths.stepped!({ align: 1 }),
        points: { show: false },
      })),
    ],
    hooks: {
      setCursor: [
        (u) => {
          const left = u.cursor.left;
          cursorAt.value = left == null || left < 0 ? null : u.posToVal(left, "x") * 1000;
        },
      ],
      draw: [(u) => drawEnds(u, theme.bg)],
    },
  }),
});
// Another run is another span: a zoom into the last one means nothing.
watch(() => props.domain[0], resetZoom);
</script>

<template>
  <figure class="tc" :data-chart="chart.key">
    <figcaption class="tc-head">
      <span class="tc-title">{{ chart.title }}</span>
      <span v-if="headline !== null" class="tc-value">{{ headline }}</span>
    </figcaption>
    <PlotLegend
      v-if="chart.lines.length > 1"
      v-model:shown="shown"
      class="tc-legend"
      :entries="legend"
    />
    <div
      ref="box"
      class="tc-plot"
      role="img"
      :aria-label="`${chart.title} over the run${cursorAt === null ? '' : `, at ${formatClock(readAt)}`}`"
    />
  </figure>
</template>

<style>
.tc {
  margin: 0;
  padding: 8px 10px 4px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  min-width: 0;
}
.tc-head {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  font-size: var(--datalib-font-size-small);
}
.tc-title {
  color: var(--datalib-muted);
}
.tc-value {
  color: var(--datalib-fg);
  font-variant-numeric: tabular-nums;
  font-weight: 600;
}
.tc-legend {
  margin-top: 4px;
}
.tc-plot {
  width: 100%;
  margin-top: 2px;
}
</style>
