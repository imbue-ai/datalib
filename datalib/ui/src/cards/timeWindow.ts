// The time window a chart shows, as a person types it into the Time
// series card's window box: a duration (`6h`, `90d`), `all`, or two
// dates (`2369-04-14 07:00 to 2369-04-14 09:00`) read in the clock's
// zone. Pure: what a text means, what to write back, and what it spans.

import { instantOfInput, inputOfInstant, type Zone } from "@/config/timeView";

export type TimeWindow =
  | { kind: "last"; ms: number }
  | { kind: "all" }
  | { kind: "between"; fromMs: number; toMs: number };

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

/// Each unit a duration can be typed in, its longest spelling first so
/// a label reads it; `m` is minutes and `mo` months, as in Grafana.
const UNITS: { names: string[]; ms: number; label: string }[] = [
  { names: ["s", "sec", "second", "seconds"], ms: 1000, label: "second" },
  { names: ["m", "min", "mins", "minute", "minutes"], ms: MIN, label: "minute" },
  { names: ["h", "hr", "hrs", "hour", "hours"], ms: HOUR, label: "hour" },
  { names: ["d", "day", "days"], ms: DAY, label: "day" },
  { names: ["w", "wk", "week", "weeks"], ms: 7 * DAY, label: "week" },
  { names: ["mo", "month", "months"], ms: 30 * DAY, label: "month" },
  { names: ["y", "yr", "year", "years"], ms: 365 * DAY, label: "year" },
];

/// The units a duration is written back and labelled in, largest first.
/// Weeks and months are left out so `90d` reads back as `90d`, not `3mo`.
const WRITTEN = UNITS.filter((u) => !["w", "mo"].includes(u.names[0]!)).reverse();
const unitOf = (ms: number) => WRITTEN.find((u) => ms >= u.ms && Number.isInteger(ms / u.ms));

/// What the window box offers when it is focused.
export const PRESETS: string[] = [
  "5m",
  "15m",
  "30m",
  "1h",
  "3h",
  "6h",
  "12h",
  "1d",
  "7d",
  "30d",
  "90d",
  "1y",
  "all",
];

export const DEFAULT_WINDOW = "90d";

const DURATION = /^(?:last\s+)?(\d+(?:\.\d+)?)\s*([a-z]+)$/;
const DATE_ONLY = /^(\d{4})-(\d{2})-(\d{2})$/;
const SEPARATOR = /\s+to\s+|\s*\.\.\s*|\s+[–—]\s+/;

/// What `text` asks for, or null when it reads as none of the three.
/// A bare date is its whole day: midnight as a start, the next midnight
/// as an end.
export function parseWindow(text: string, zone: Zone): TimeWindow | null {
  const t = text.trim().toLowerCase();
  if (t === "all") return { kind: "all" };
  const d = DURATION.exec(t);
  if (d) {
    const unit = UNITS.find((u) => u.names.includes(d[2]!));
    const ms = unit ? Number(d[1]) * unit.ms : 0;
    return ms > 0 ? { kind: "last", ms } : null;
  }
  const parts = text.trim().split(SEPARATOR);
  if (parts.length !== 2) return null;
  const from = instantOf(parts[0]!, zone, "start");
  const to = instantOf(parts[1]!, zone, "end");
  return from !== null && to !== null && to > from
    ? { kind: "between", fromMs: from, toMs: to }
    : null;
}

function instantOf(text: string, zone: Zone, edge: "start" | "end"): number | null {
  const t = text.trim();
  const day = DATE_ONLY.exec(t);
  if (day) {
    const shift = edge === "end" ? 1 : 0;
    const next = new Date(Date.UTC(+day[1]!, +day[2]! - 1, +day[3]! + shift));
    return instantOfInput(`${next.toISOString().slice(0, 10)}T00:00`, zone)?.ms ?? null;
  }
  // A stamp that names its own offset (as the card stores one) means
  // that instant whatever the zone.
  if (/(z|[+-]\d{2}:\d{2})$/i.test(t)) {
    const ms = Date.parse(t.replace(" ", "T"));
    return Number.isFinite(ms) ? ms : null;
  }
  return instantOfInput(t.replace(" ", "T"), zone)?.ms ?? null;
}

/// A duration in its shortest spelling: `90d`, `1.5h`.
function durationText(ms: number): string {
  const unit = unitOf(ms);
  if (unit) return `${ms / unit.ms}${unit.names[0]}`;
  return `${Number((ms / MIN).toPrecision(6))}m`;
}

/// `w` as the box shows it, under `zone`; it reads back as `w`.
export function windowText(w: TimeWindow, zone: Zone): string {
  if (w.kind === "all") return "all";
  if (w.kind === "last") return durationText(w.ms);
  const at = (ms: number) => inputOfInstant({ ms, offsetMin: null }, zone).slice(0, 16);
  return `${at(w.fromMs).replace("T", " ")} to ${at(w.toMs).replace("T", " ")}`;
}

/// `w` as the card's state keeps it: a range in UTC, so another zone on
/// the clock still reads it as the same two instants.
export function storedText(w: TimeWindow): string {
  if (w.kind !== "between") return windowText(w, { kind: "named", name: "UTC" });
  const iso = (ms: number) => new Date(ms).toISOString();
  return `${iso(w.fromMs)} to ${iso(w.toMs)}`;
}

/// What a preset says in the list: `Last 6 hours`, `All`.
export function windowLabel(w: TimeWindow): string {
  if (w.kind === "all") return "All";
  if (w.kind === "between") return "Between two dates";
  const unit = unitOf(w.ms);
  if (!unit) return `Last ${durationText(w.ms)}`;
  const n = w.ms / unit.ms;
  return `Last ${n} ${unit.label}${n === 1 ? "" : "s"}`;
}

/// The span `w` covers in milliseconds: a duration ends at `newestMs`,
/// the newest point picked; null for all of it.
export function windowSpan(
  w: TimeWindow,
  newestMs: number,
): { fromMs: number; toMs: number } | null {
  if (w.kind === "all") return null;
  if (w.kind === "between") return { fromMs: w.fromMs, toMs: w.toMs };
  return { fromMs: newestMs - w.ms, toMs: newestMs };
}

/// The span `w` covers when starts are aligned, given the longest line:
/// a duration is the first that much of each line. A date range names
/// clock times, which aligned lines no longer have, so it shows them
/// whole.
export function alignedWindowSpan(
  w: TimeWindow,
  longestMs: number,
): { fromMs: number; toMs: number } {
  return { fromMs: 0, toMs: w.kind === "last" ? Math.min(w.ms, longestMs) : longestMs };
}
