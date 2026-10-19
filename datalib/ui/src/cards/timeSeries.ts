// The rules behind the Time series card (TimeSeriesCard.ce.vue), pure:
// what the applet answers, which time series share an axis, what unit a
// value is shown in, and where on the chart a point lands. The applet is
// `unified_time_series_index` (docs/dev/applets.md); units are UCUM codes
// it has already read (docs/dev/time_series.md).

export const TIME_SERIES_APPLET = "/applet/unified_time_series_index";

export type TimeKind = "instant" | "day";
export type ValueType = "integer" | "float" | "text";

/// One time series as `GET /time_series` lists it. `t` is unix
/// milliseconds for an `instant` and days since 1970-01-01 for a `day`.
/// A value times `unit_factor` plus `unit_offset` is in the SI unit of
/// its `quantity`; two time series of one quantity share an axis.
export type Listed = {
  time_series_id: string;
  source_id: string;
  about_uuid: string | null;
  about_name: string | null;
  name: string;
  human_name: string;
  unit: string;
  unit_label: string;
  quantity: string;
  unit_factor: number;
  unit_offset: number;
  value_type: ValueType;
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

/// How an axis shows a value: `label` is the unit it is shown in, and a
/// value in SI units times `factor` plus `offset` is the number shown.
export type Shown = { label: string; factor: number; offset?: number };

const shown = (v: number, unit: Shown) => v * unit.factor + (unit.offset ?? 0);

const DAY_S = 86_400;
/// UCUM's `a`, the Julian year.
const YEAR_S = 365.25 * DAY_S;

/// The unit an axis of `quantity` is shown in, given its largest value
/// in SI units: a duration or a distance in whatever suits its size (a
/// chart of sleep reads in hours, an age in years), anything else in the unit of `first`,
/// the first time series on the axis.
export function displayUnit(quantity: string, first: Listed, maxAbs: number): Shown {
  if (quantity === "T1") {
    if (maxAbs >= 2 * YEAR_S) return { label: "yr", factor: 1 / YEAR_S };
    if (maxAbs >= 2 * DAY_S) return { label: "d", factor: 1 / DAY_S };
    if (maxAbs >= 2 * 3600) return { label: "h", factor: 1 / 3600 };
    if (maxAbs >= 120) return { label: "min", factor: 1 / 60 };
    if (maxAbs < 1) return { label: "ms", factor: 1000 };
    return { label: "s", factor: 1 };
  }
  if (quantity === "L1") {
    return maxAbs >= 2000 ? { label: "km", factor: 1 / 1000 } : { label: "m", factor: 1 };
  }
  return {
    label: first.unit_label,
    factor: 1 / first.unit_factor,
    offset: first.unit_offset === 0 ? 0 : -first.unit_offset / first.unit_factor,
  };
}

/// One axis per quantity, the first picked on the left: two at most,
/// since a third scale on one chart is one nobody can read.
export const MAX_AXES = 2;

export function quantitiesOf(picked: Listed[]): string[] {
  const out: string[] = [];
  for (const p of picked) if (!out.includes(p.quantity)) out.push(p.quantity);
  return out;
}

/// What the list says a time series is in.
export function unitText(s: Listed): string {
  if (s.value_type === "text") return "text";
  if (s.quantity === "T1") return "duration";
  if (s.quantity === "L1") return "distance";
  return s.unit_label || "no unit";
}

/// Why `candidate` cannot join `picked`, or null when it can.
export function refusal(picked: Listed[], candidate: Listed): string | null {
  if (candidate.value_type === "text") {
    return `${candidate.human_name} is text, which the chart cannot draw yet.`;
  }
  const quantities = quantitiesOf(picked);
  if (quantities.includes(candidate.quantity) || quantities.length < MAX_AXES) return null;
  const named = quantities
    .map((q) => unitText(picked.find((p) => p.quantity === q)!))
    .join(" and ");
  return `The chart already has two axes (${named}); take one off to add ${unitText(candidate)}.`;
}

/// What a chart calls each line: whichever of what it measures and what
/// it is about tells it apart from the others drawn with it. Five YoLink
/// temperatures are five devices; one run's heart rate and speed are two
/// measures.
export function lineLabels(picked: Listed[]): Map<string, string> {
  const about = (s: Listed) => s.about_name ?? s.source_id;
  const varies = (f: (s: Listed) => string) => new Set(picked.map(f)).size > 1;
  const byAbout = varies(about);
  const byMeasure = varies((s) => s.human_name) || !byAbout;
  return new Map(
    picked.map((s) => [
      s.time_series_id,
      [byMeasure ? s.human_name : null, byAbout ? about(s) : null].filter(Boolean).join(" · "),
    ]),
  );
}

/// The time series about one document (an activity), listed together.
export type About = { uuid: string; name: string; items: Listed[] };

/// The time series that answer every word of `filter` in their human
/// name, name, source or what they are about, grouped by source in
/// source order. Within a source, the ones about the source as a whole
/// come first, then one group per document, newest first.
export function grouped(
  all: Listed[],
  filter: string,
): { source: string; items: Listed[]; abouts: About[] }[] {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  const out = new Map<string, { items: Listed[]; abouts: Map<string, About> }>();
  for (const s of all) {
    const hay = `${s.human_name} ${s.name} ${s.source_id} ${s.about_name ?? ""}`.toLowerCase();
    if (!words.every((w) => hay.includes(w))) continue;
    const group = out.get(s.source_id) ?? {
      items: [] as Listed[],
      abouts: new Map<string, About>(),
    };
    out.set(s.source_id, group);
    if (s.about_uuid === null) {
      group.items.push(s);
      continue;
    }
    const about = group.abouts.get(s.about_uuid) ?? {
      uuid: s.about_uuid,
      name: s.about_name ?? s.about_uuid,
      items: [],
    };
    about.items.push(s);
    group.abouts.set(s.about_uuid, about);
  }
  const newest = (a: About) => Math.max(...a.items.map((s) => toMs(s.time_kind, s.last_t ?? 0)));
  return [...out.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([source, g]) => ({
      source,
      items: g.items,
      abouts: [...g.abouts.values()].sort((a, b) => newest(b) - newest(a)),
    }));
}

export type XY = { x: number; y: number };
export type Band = { x: number; lo: number; hi: number };

const si = (a: Listed, v: number) => v * a.unit_factor + a.unit_offset;

/// A time series' line, in milliseconds and SI units: every point with a
/// value, or every bucket's mean.
export function lineOf(a: Answered): XY[] {
  if (a.kind === "raw") {
    return a.points.flatMap((p) =>
      p.value === null ? [] : [{ x: toMs(a.time_kind, p.t), y: si(a, p.value) }],
    );
  }
  return a.buckets.flatMap((b) =>
    b.mean === null ? [] : [{ x: toMs(a.time_kind, b.t), y: si(a, b.mean) }],
  );
}

/// The spread a bucketed answer hides, min to max per bucket, in SI
/// units; null when the answer has every point.
export function bandOf(a: Answered): Band[] | null {
  if (a.kind === "raw") return null;
  return a.buckets.flatMap((b) =>
    b.min === null || b.max === null
      ? []
      : [{ x: toMs(a.time_kind, b.t), lo: si(a, b.min), hi: si(a, b.max) }],
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

/// A label on the time axis: as [`formatTime`] for the first, but only
/// the time of day after it when the span is under two days, since
/// every tick is then on the first one's date or the next.
export function formatTimeTick(ms: number, kind: TimeKind, spanMs: number, first: boolean): string {
  if (first || kind === "day" || spanMs >= 2 * DAY_MS) return formatTime(ms, kind, spanMs);
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/// A value as a tooltip writes it, in its display unit.
export function formatValue(v: number, unit: Shown): string {
  const n = shown(v, unit);
  const abs = Math.abs(n);
  const text = abs >= 100 ? n.toFixed(0) : abs >= 10 ? n.toFixed(1) : n.toFixed(2);
  return unit.label ? `${text} ${unit.label}` : text;
}

/// A tick on a value axis, in its display unit, with no trailing zeros:
/// ticks are round numbers already.
export function formatTick(v: number, unit: Shown): string {
  return String(Number(shown(v, unit).toPrecision(6)));
}

/// Round ticks in the display unit for an axis spanning `[lo, hi]` in SI
/// units, returned in SI units.
export function axisTicks(lo: number, hi: number, unit: Shown): number[] {
  const offset = unit.offset ?? 0;
  return niceTicks(shown(lo, unit), shown(hi, unit)).map((t) => (t - offset) / unit.factor);
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
