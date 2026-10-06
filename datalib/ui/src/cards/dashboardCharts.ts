// The sync dashboard's charts, decided as values: which charts a step
// gets from the series it reported, and the arithmetic every chart
// shares — reading a step function at an instant, summing series,
// scaling a domain. The card draws what this returns.
import type { DashboardStep, Sample } from "@/api";

/// A point in milliseconds, for arithmetic; the wire carries ISO stamps.
export type Point = { t: number; v: number };

export type Line = {
  label: string;
  /// Categorical slot (0-based), or a status colour for warnings and
  /// errors. Follows the series, never its rank.
  color: { slot: number } | { status: "warn" | "error" } | { accent: true };
  points: Point[];
};

export type Chart = {
  key: string;
  title: string;
  /// `bytes` draws sizes; anything else, counts.
  unit: "bytes" | "count";
  /// A running total or a queue starts at nothing; a size does not, and
  /// is scaled to its own range so a small change still shows.
  fromZero: boolean;
  lines: Line[];
};

/// The most lines one chart draws apart; past it, the rest are summed
/// into "other". Six categorical slots pass the palette checks side by
/// side (`dataviz` validator, light and dark).
export const MAX_LINES = 6;

/// Series a step reports that are not about this run: the whole store's
/// problem counts, which the Name column's badges already show.
const NOT_OF_THE_RUN = new Set(["problems"]);

export function toPoints(samples: Sample[]): Point[] {
  return samples
    .map((s) => ({ t: Date.parse(s.at), v: s.value }))
    .filter((p) => Number.isFinite(p.t));
}

/// A step function's value at `t`: the last point at or before it, or
/// null before the first.
export function valueAt(points: Point[], t: number): number | null {
  let out: number | null = null;
  for (const p of points) {
    if (p.t > t) break;
    out = p.v;
  }
  return out;
}

/// Several step functions added up, as one, with a point wherever any
/// of them moves. A series not yet begun counts as nothing.
export function sumSeries(series: Point[][]): Point[] {
  const times = [...new Set(series.flatMap((s) => s.map((p) => p.t)))].sort((a, b) => a - b);
  return times.map((t) => ({
    t,
    v: series.reduce((acc, s) => acc + (valueAt(s, t) ?? 0), 0),
  }));
}

/// `rows_upserted_total` → "rows upserted": the suffix says the series
/// is a running total, which every chart but the queue's is.
export function humanName(name: string): string {
  return name.replace(/_total$/, "").replace(/_/g, " ");
}

/// `table=messages,kind=dm` → "messages, dm": the values say which is
/// which; the keys are the same on every line of a chart.
export function labelText(labels: string): string {
  if (!labels) return "total";
  return labels
    .split(",")
    .map((kv) => kv.slice(kv.indexOf("=") + 1))
    .join(", ");
}

/// A queue's lines: `queued` of its own and one per producer, summed.
export function queueLine(step: DashboardStep): Point[] | null {
  const queued = step.series.filter((s) => s.name === "queued");
  if (queued.length === 0) return null;
  return sumSeries(queued.map((s) => toPoints(s.points)));
}

/// Every chart a step gets, in the order they read best: what is ahead
/// of it, what it has done, what went wrong, what it weighs.
export function stepCharts(step: DashboardStep): Chart[] {
  const charts: Chart[] = [];
  const queue = queueLine(step);
  if (queue) {
    charts.push({
      key: "queued",
      title: "queued",
      unit: "count",
      fromZero: true,
      lines: [{ label: "queued", color: { accent: true }, points: queue }],
    });
  }
  const names = [...new Set(step.series.map((s) => s.name))].filter(
    (n) => n !== "queued" && !NOT_OF_THE_RUN.has(n),
  );
  for (const name of names) {
    const series = step.series.filter((s) => s.name === name);
    const kept = series.slice(0, series.length > MAX_LINES ? MAX_LINES - 1 : MAX_LINES);
    const rest = series.slice(kept.length);
    const lines: Line[] = kept.map((s, i) => ({
      label: labelText(s.labels),
      color: series.length === 1 ? { accent: true } : { slot: i },
      points: toPoints(s.points),
    }));
    if (rest.length > 0) {
      lines.push({
        label: `${rest.length} more`,
        color: { slot: MAX_LINES - 1 },
        points: sumSeries(rest.map((s) => toPoints(s.points))),
      });
    }
    charts.push({ key: name, title: humanName(name), unit: "count", fromZero: true, lines });
  }
  if (step.warnings.length > 0 || step.errors.length > 0) {
    charts.push(problemChart(step.warnings, step.errors));
  }
  if (weighsSomething(step.disk)) charts.push(diskChart(step.disk));
  return charts;
}

export function problemChart(warnings: Sample[], errors: Sample[]): Chart {
  return {
    key: "log_problems",
    title: "warnings and errors logged",
    unit: "count",
    fromZero: true,
    lines: [
      { label: "warnings", color: { status: "warn" }, points: toPoints(warnings) },
      { label: "errors", color: { status: "error" }, points: toPoints(errors) },
    ],
  };
}

/// A tree that held nothing the whole run — a step that writes outside
/// its own, or has not written yet — is a flat zero, not worth a chart.
const weighsSomething = (disk: Sample[]) => disk.some((s) => s.value > 0);

export function diskChart(disk: Sample[]): Chart {
  return {
    key: "disk",
    title: "size on disk",
    unit: "bytes",
    fromZero: false,
    lines: [{ label: "size", color: { accent: true }, points: toPoints(disk) }],
  };
}

/// The group's own charts: its steps' queues and log lines added up, and
/// its folder's size.
export function groupCharts(steps: DashboardStep[], disk: Sample[]): Chart[] {
  const charts: Chart[] = [];
  const queues = steps.map(queueLine).filter((q): q is Point[] => q !== null);
  if (queues.length > 0) {
    charts.push({
      key: "queued",
      title: "queued, every step",
      unit: "count",
      fromZero: true,
      lines: [{ label: "queued", color: { accent: true }, points: sumSeries(queues) }],
    });
  }
  const warnings = sumSeries(steps.map((s) => toPoints(s.warnings)));
  const errors = sumSeries(steps.map((s) => toPoints(s.errors)));
  if (warnings.length > 0 || errors.length > 0) {
    charts.push({
      ...problemChart([], []),
      lines: [
        { label: "warnings", color: { status: "warn" }, points: warnings },
        { label: "errors", color: { status: "error" }, points: errors },
      ],
    });
  }
  if (weighsSomething(disk)) charts.push(diskChart(disk));
  return charts;
}

/// The time the charts span: from the first of the group's steps to
/// start in the run to the last to finish, or now while one still runs.
/// The group's own stretch, not the run's: a sync of everything can run
/// an hour while this source took thirty seconds of it. And the run's
/// start can be a pinned clock (`--now`) while the steps stamp the wall
/// clock, which would put every point at one edge.
export function groupSpan(
  steps: DashboardStep[],
  run: { started_at_utc: string; finished_at_utc: string | null } | null,
  now: number,
): [number, number] {
  const ran = steps.filter((s) => s.in_run);
  const starts = ran.flatMap((s) => (s.started_at_utc ? [Date.parse(s.started_at_utc)] : []));
  const running = ran.some((s) => s.started_at_utc && !s.finished_at_utc);
  const ends = ran.flatMap((s) => (s.finished_at_utc ? [Date.parse(s.finished_at_utc)] : []));
  const start = starts.length
    ? Math.min(...starts)
    : run
      ? Date.parse(run.started_at_utc)
      : now - 60_000;
  const end =
    running || ends.length === 0
      ? run?.finished_at_utc && !running
        ? Date.parse(run.finished_at_utc)
        : now
      : Math.max(...ends);
  return [start, Math.max(end, start + 1000)];
}

/// The value range a chart's y-axis covers. From zero for a count; a
/// size to its own range, padded, so a change of a few kB in a GB tree
/// is not a flat line.
export function yRange(chart: Chart, domain: [number, number]): [number, number] {
  const values = chart.lines.flatMap((l) => {
    const before = valueAt(l.points, domain[0]);
    const inside = l.points.filter((p) => p.t >= domain[0] && p.t <= domain[1]).map((p) => p.v);
    return before === null ? inside : [before, ...inside];
  });
  if (values.length === 0) return [0, 1];
  const max = Math.max(...values);
  const min = chart.fromZero ? 0 : Math.min(...values);
  if (max === min) return chart.fromZero ? [0, Math.max(1, max)] : [min - 1, max + 1];
  const pad = chart.fromZero ? 0 : (max - min) * 0.1;
  return [min - pad, max + pad];
}

/// A chart's lines as uPlot draws them: one shared x column (seconds,
/// uPlot's time unit) and a y column per line, read as a step function
/// at every instant any line moves. Points before the domain are folded
/// into its start, so a line that began earlier starts at its value
/// then; a line is null before its first point, so it starts where it
/// starts; and every line is carried on to `end` — the step's finish,
/// or now — so the last value reaches the edge it holds until.
export function aligned(
  chart: Chart,
  domain: [number, number],
  end: number,
): { xs: number[]; ys: (number | null)[][] } {
  const stop = Math.min(end, domain[1]);
  const inside = chart.lines.flatMap((l) =>
    l.points.filter((p) => p.t > domain[0] && p.t <= stop).map((p) => p.t),
  );
  const times = [...new Set([domain[0], ...inside, stop])].sort((a, b) => a - b);
  return {
    xs: times.map((t) => t / 1000),
    ys: chart.lines.map((l) => times.map((t) => valueAt(l.points, t))),
  };
}
