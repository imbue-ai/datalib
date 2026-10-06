<script setup lang="ts">
// One small time chart on the sync dashboard, drawn by uPlot: a step
// function per line over the run. Every chart on the card shares the
// time axis and, through uPlot's cursor sync, one crosshair. The header
// reads the value under the crosshair, or the latest when there is none;
// a legend names each line, with its value, when there is more than
// one. Which charts, and the arithmetic, are `dashboardCharts.ts`.
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import uPlot from "uplot";
import { formatBytes } from "@/config/bytes";
import { formatClock } from "@/config/timeFormat";
import { aligned, valueAt, yRange, type Chart, type Line } from "./dashboardCharts";

const props = defineProps<{
  chart: Chart;
  /// The span every chart on the card covers, in ms.
  domain: [number, number];
  /// Where the lines stop being carried forward: the step's finish, or now.
  end: number;
  /// Charts with the same key share one crosshair.
  syncKey: string;
}>();

const HEIGHT = 96;

const box = ref<HTMLElement | null>(null);
let plot: uPlot | null = null;
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

/// A line's colour as the theme token it is.
function colorVar(l: Line): string {
  const c = l.color;
  if ("slot" in c) return `--viz-series-${c.slot + 1}`;
  if ("status" in c) return c.status === "warn" ? "--datalib-log-warn" : "--datalib-log-error";
  return "--datalib-accent";
}

/// A token's value now: uPlot draws on a canvas, which takes a colour,
/// not a `var()`.
function resolve(token: string): string {
  return box.value ? getComputedStyle(box.value).getPropertyValue(token).trim() : "";
}

function data(): uPlot.AlignedData {
  const { xs, ys } = aligned(props.chart, props.domain, props.end);
  return [xs, ...ys];
}

/// The last drawn point of each line, ringed: a line with one sample is
/// otherwise a line of no length, and the dot says where it stands now.
function drawEnds(u: uPlot) {
  const ctx = u.ctx;
  const xs = u.data[0];
  u.series.forEach((s, i) => {
    if (i === 0 || !s.show) return;
    const ys = u.data[i];
    let last = ys.length - 1;
    while (last >= 0 && ys[last] == null) last--;
    if (last < 0) return;
    const x = u.valToPos(xs[last], "x", true);
    const y = u.valToPos(ys[last] as number, "y", true);
    ctx.beginPath();
    ctx.arc(x, y, 3 * uPlot.pxRatio, 0, 2 * Math.PI);
    ctx.fillStyle = s.stroke as unknown as string;
    ctx.fill();
    ctx.lineWidth = 1.5 * uPlot.pxRatio;
    ctx.strokeStyle = resolve("--datalib-bg");
    ctx.stroke();
  });
}

function options(width: number): uPlot.Options {
  const muted = resolve("--datalib-muted");
  const grid = resolve("--datalib-border");
  const font = "10px system-ui, sans-serif";
  return {
    width,
    height: HEIGHT,
    legend: { show: false },
    cursor: {
      sync: { key: props.syncKey },
      points: { show: false },
      drag: { x: false, y: false },
    },
    scales: {
      x: { time: true, range: () => [props.domain[0] / 1000, props.domain[1] / 1000] },
      y: { range: () => yRange(props.chart, props.domain) },
    },
    axes: [
      {
        stroke: muted,
        font,
        size: 20,
        space: 72,
        grid: { show: false },
        ticks: { stroke: grid, width: 1, size: 4 },
        values: (_u, splits) => splits.map((s) => formatClock(s * 1000)),
      },
      {
        stroke: muted,
        font,
        size: 46,
        space: 22,
        grid: { stroke: grid, width: 1 },
        ticks: { show: false },
        values: (_u, splits) => splits.map((v) => format(v)),
      },
    ],
    series: [
      {},
      ...props.chart.lines.map((l) => ({
        label: l.label,
        stroke: resolve(colorVar(l)),
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
      draw: [drawEnds],
    },
  };
}

function build() {
  plot?.destroy();
  plot = null;
  if (!box.value) return;
  plot = new uPlot(options(Math.max(120, box.value.clientWidth)), data(), box.value);
}

/// The shape a plot is built for: another line, a renamed one or another
/// unit needs a new plot; anything else is new data on the old one.
const shape = computed(() =>
  JSON.stringify([props.chart.unit, props.chart.lines.map((l) => [l.label, l.color])]),
);
watch(shape, build);
watch(
  () => [props.chart, props.domain, props.end],
  () => plot?.setData(data()),
  { deep: true },
);

let observer: ResizeObserver | null = null;
let scheme: MediaQueryList | null = null;
onMounted(() => {
  build();
  if (!box.value) return;
  observer = new ResizeObserver(([entry]) => {
    plot?.setSize({ width: Math.max(120, Math.floor(entry.contentRect.width)), height: HEIGHT });
  });
  observer.observe(box.value);
  // The colours are read once per plot; a theme change rebuilds it.
  scheme = window.matchMedia("(prefers-color-scheme: dark)");
  scheme.addEventListener("change", build);
});
onUnmounted(() => {
  observer?.disconnect();
  scheme?.removeEventListener("change", build);
  plot?.destroy();
  plot = null;
});
</script>

<template>
  <figure class="tc" :data-chart="chart.key">
    <figcaption class="tc-head">
      <span class="tc-title">{{ chart.title }}</span>
      <span v-if="headline !== null" class="tc-value">{{ headline }}</span>
    </figcaption>
    <ul v-if="chart.lines.length > 1" class="tc-legend">
      <li v-for="l in chart.lines" :key="l.label">
        <span class="tc-swatch" :style="{ background: `var(${colorVar(l)})` }" />
        <span class="tc-label">{{ l.label }}</span>
        <span class="tc-legend-value">{{ format(valueOf(l)) }}</span>
      </li>
    </ul>
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
  display: flex;
  flex-wrap: wrap;
  gap: 2px 10px;
  margin: 4px 0 0;
  padding: 0;
  list-style: none;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-fg);
}
.tc-legend li {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.tc-swatch {
  width: 10px;
  height: 3px;
  border-radius: 2px;
}
.tc-label {
  color: var(--datalib-muted);
}
.tc-legend-value {
  font-variant-numeric: tabular-nums;
}
.tc-plot {
  width: 100%;
  margin-top: 2px;
}
/* uPlot's crosshair: recessive, dashed, the theme's muted ink. */
.tc-plot .u-cursor-x {
  border-right: 1px dashed var(--datalib-muted);
}
.tc-plot .u-cursor-y {
  display: none;
}
</style>
