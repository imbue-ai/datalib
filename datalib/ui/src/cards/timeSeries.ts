// The rules behind the Time series card (TimeSeriesCard.ce.vue), pure:
// what the applet answers, which time series share an axis, what unit a
// value is shown in, and where on the chart a point lands. The applet is
// `unified_time_series_index` (docs/dev/applets.md).

export const TIME_SERIES_APPLET = "/applet/unified_time_series_index";

export type TimeKind = "instant" | "day";

/// One time series as `GET /time_series` lists it. `t` is unix
/// milliseconds for an `instant` and days since 1970-01-01 for a `day`.
export type Listed = {
  time_series_id: string;
  source_id: string;
  entity: string;
  metric: string;
  label: string;
  unit: string;
  time_kind: TimeKind;
  point_count: number;
  first_t: number | null;
  last_t: number | null;
};

export type RawPoint = { t: number; value: number | null; text: string | null };
export type Bucket = {
  t: number;
  min: number | null;
  max: number | null;
  mean: number | null;
  count: number;
};

/// One time series as `POST /points` answers it.
export type Answered = Listed &
  ({ kind: "raw"; points: RawPoint[] } | { kind: "buckets"; width: number; buckets: Bucket[] });

const DAY_MS = 86_400_000;

export function toMs(kind: TimeKind, t: number): number {
  return kind === "day" ? t * DAY_MS : t;
}

export function fromMs(kind: TimeKind, ms: number): number {
  return kind === "day" ? Math.floor(ms / DAY_MS) : Math.round(ms);
}

export type RangeKey = "30d" | "90d" | "1y" | "all";

export const RANGES: { key: RangeKey; label: string; days: number | null }[] = [
  { key: "30d", label: "30 d", days: 30 },
  { key: "90d", label: "90 d", days: 90 },
  { key: "1y", label: "1 y", days: 365 },
  { key: "all", label: "All", days: null },
];

/// The span a range shows, ending at the newest point of what is picked;
/// null for all of it.
export function rangeBounds(
  key: RangeKey,
  newestMs: number,
): { fromMs: number; toMs: number } | null {
  const days = RANGES.find((r) => r.key === key)?.days ?? null;
  if (days === null) return null;
  return { fromMs: newestMs - days * DAY_MS, toMs: newestMs };
}

/// The newest point any of `picked` holds, in milliseconds; null when
/// none holds one.
export function newestMs(picked: Listed[]): number | null {
  const all = picked.flatMap((p) => (p.last_t === null ? [] : [toMs(p.time_kind, p.last_t)]));
  return all.length ? Math.max(...all) : null;
}

/// The words the stored unit is shown in, and what a stored value is
/// multiplied by to get there. Durations are stored in seconds and
/// distances in metres; a chart of sleep reads in hours.
export function displayUnit(unit: string, maxAbs: number): { label: string; factor: number } {
  if (unit === "s") {
    if (maxAbs >= 2 * 3600) return { label: "h", factor: 1 / 3600 };
    if (maxAbs >= 120) return { label: "min", factor: 1 / 60 };
    if (maxAbs < 1) return { label: "ms", factor: 1000 };
    return { label: "s", factor: 1 };
  }
  if (unit === "m" && maxAbs >= 2000) return { label: "km", factor: 1 / 1000 };
  return { label: unit, factor: 1 };
}

/// One axis per unit, the first picked on the left: two at most, since a
/// third scale on one chart is one nobody can read.
export const MAX_UNITS = 2;

export function unitsOf(picked: Listed[]): string[] {
  const out: string[] = [];
  for (const p of picked) if (!out.includes(p.unit)) out.push(p.unit);
  return out;
}

/// Why `candidate` cannot join `picked`, or null when it can.
export function refusal(picked: Listed[], candidate: Listed): string | null {
  const units = unitsOf(picked);
  if (units.includes(candidate.unit) || units.length < MAX_UNITS) return null;
  const shown = units.map((u) => u || "no unit").join(" and ");
  return `The chart already has two units (${shown}); take one off to add ${candidate.unit || "a unit-less time series"}.`;
}

/// The time series that answer every word of `filter` in their label,
/// metric, entity or source, grouped by source in source order.
export function grouped(all: Listed[], filter: string): { source: string; items: Listed[] }[] {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  const out = new Map<string, Listed[]>();
  for (const s of all) {
    const hay = `${s.label} ${s.metric} ${s.entity} ${s.source_id}`.toLowerCase();
    if (!words.every((w) => hay.includes(w))) continue;
    const list = out.get(s.source_id) ?? [];
    list.push(s);
    out.set(s.source_id, list);
  }
  return [...out.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([source, items]) => ({ source, items }));
}

export type XY = { x: number; y: number };
export type Band = { x: number; lo: number; hi: number };

/// A time series' line, in milliseconds and stored units: every point
/// with a value, or every bucket's mean.
export function lineOf(a: Answered): XY[] {
  if (a.kind === "raw") {
    return a.points.flatMap((p) =>
      p.value === null ? [] : [{ x: toMs(a.time_kind, p.t), y: p.value }],
    );
  }
  return a.buckets.flatMap((b) =>
    b.mean === null ? [] : [{ x: toMs(a.time_kind, b.t), y: b.mean }],
  );
}

/// The spread a bucketed answer hides, min to max per bucket; null when
/// the answer has every point.
export function bandOf(a: Answered): Band[] | null {
  if (a.kind === "raw") return null;
  return a.buckets.flatMap((b) =>
    b.min === null || b.max === null ? [] : [{ x: toMs(a.time_kind, b.t), lo: b.min, hi: b.max }],
  );
}

/// About `count` round values covering `[lo, hi]`, ascending.
export function niceTicks(lo: number, hi: number, count = 4): number[] {
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return [];
  if (lo === hi) return [lo];
  const raw = (hi - lo) / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? 10 * mag;
  const out: number[] = [];
  for (let v = Math.ceil(lo / step) * step; v <= hi + step * 1e-9; v += step) {
    out.push(Number(v.toPrecision(12)));
  }
  return out;
}

/// The value range an axis covers: the data's, padded a little so a line
/// never sits on the frame, and widened to the ticks.
export function extent(values: number[]): [number, number] | null {
  const finite = values.filter(Number.isFinite);
  if (!finite.length) return null;
  let lo = Math.min(...finite);
  let hi = Math.max(...finite);
  if (lo === hi) {
    const pad = Math.abs(lo) * 0.05 || 1;
    return [lo - pad, hi + pad];
  }
  const pad = (hi - lo) * 0.05;
  lo -= pad;
  hi += pad;
  const ticks = niceTicks(lo, hi);
  return [Math.min(lo, ticks[0] ?? lo), Math.max(hi, ticks.at(-1) ?? hi)];
}

/// A date as the chart writes it: the day for a span of days or more,
/// else the day and the time. Day points are calendar days, so they are
/// read in UTC, where `toMs` put them.
export function formatTime(ms: number, kind: TimeKind, spanMs: number): string {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  if (kind === "day") {
    return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
  }
  const day = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  return spanMs >= 2 * DAY_MS ? day : `${day} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/// A value as a tooltip writes it, in its display unit.
export function formatValue(v: number, unit: { label: string; factor: number }): string {
  const shown = v * unit.factor;
  const abs = Math.abs(shown);
  const text = abs >= 100 ? shown.toFixed(0) : abs >= 10 ? shown.toFixed(1) : shown.toFixed(2);
  return unit.label ? `${text} ${unit.label}` : text;
}

/// A tick on a value axis, in its display unit, with no trailing zeros:
/// ticks are round numbers already.
export function formatTick(v: number, unit: { factor: number }): string {
  return String(Number((v * unit.factor).toPrecision(6)));
}

/// The point of `line` nearest `x`, by time, when one is within
/// `within`; null otherwise, so a line with no data near the cursor says
/// nothing rather than a value from weeks away.
export function nearest(line: XY[], x: number, within = Infinity): XY | null {
  let best: XY | null = null;
  for (const p of line) if (!best || Math.abs(p.x - x) < Math.abs(best.x - x)) best = p;
  return best && Math.abs(best.x - x) <= within ? best : null;
}

/// The card's state string: the picked ids in order, and the range.
export function encodeState(ids: string[], range: RangeKey): string {
  const q = new URLSearchParams();
  if (ids.length) q.set("ids", ids.join(","));
  if (range !== "90d") q.set("range", range);
  return q.toString();
}

export function decodeState(state: string): { ids: string[]; range: RangeKey } {
  const q = new URLSearchParams(state);
  const range = q.get("range");
  return {
    ids: (q.get("ids") ?? "").split(",").filter(Boolean),
    range: RANGES.some((r) => r.key === range) ? (range as RangeKey) : "90d",
  };
}
