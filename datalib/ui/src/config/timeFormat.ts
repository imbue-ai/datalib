// How this app renders one of the tree's timestamps.
//
// Two functions, and they are a pair: `formatRelative` is what a
// timestamp column *shows* ("5 minutes ago"), and `formatStamp` is what
// it reveals on hover. The question a person has about a sync is almost
// always "is this stale?", which the relative form answers directly and
// an absolute one makes you do arithmetic for; the exact instant is the
// occasional need, so it gets the gesture.
//
// Every timestamp in this project is ISO-8601 with the source's own UTC
// offset preserved (see AGENTS.md), which `Date` parses correctly
// without either side being normalized first.

/// The absolute form, in the viewer's own locale, on a 24-hour clock.
///
/// `hourCycle` rather than the locale's default: these times are
/// operational — "did this run before or after that one" — and a
/// 12-hour clock makes that a two-token comparison. Everything else
/// (field order, month name, separators) still follows the system.
const STAMP_FMT = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
});

/// Render an exact stamp. Returns the input unchanged when it isn't a
/// date we can parse — a stamp we can't read is still worth showing
/// verbatim rather than replacing with a shrug.
export function formatStamp(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : STAMP_FMT.format(d);
}

/// Order two stamps by the instant they name.
///
/// A timestamp column that *displays* "5 minutes ago" must not sort on
/// that text — alphabetically "10 seconds ago" precedes "2 hours ago"
/// precedes "just now", which is three kinds of wrong at once. AG Grid
/// sorts on the row's value rather than what a `cellRenderer` painted,
/// so the text is never the key; this is about the value.
///
/// Even then, comparing the values as strings is wrong. Every stamp
/// here is ISO-8601 with the *source's own* UTC offset preserved (see
/// AGENTS.md), so `2026-09-01T13:00:00+02:00` and
/// `2026-09-01T04:00:00-07:00` are the same instant written two ways
/// and neither text order nor equality survives it. Parse, then
/// compare.
///
/// A row that has never run sorts as **forever ago** — older than any
/// real stamp, rather than as a special case pinned to one end.
///
/// That makes this a plain total order, which is the point: reversing
/// the sort reverses the whole column, so "never run" leads ascending
/// and trails descending, and one click on the header is how you ask
/// "what has never run?". The alternative — keeping nulls at the
/// bottom whichever way the column points — needs the sort direction
/// threaded in and negated back out, which is two orders wearing one
/// function's clothes.
export function compareStamps(a: string | null, b: string | null): number {
  if (a === b) return 0;
  if (!a) return -1;
  if (!b) return 1;
  return Date.parse(a) - Date.parse(b);
}

/// `numeric: "always"` rather than `"auto"`: the auto forms are
/// friendlier in prose ("yesterday", "last week") but they collapse a
/// range into a word, and this is an operational column where "1 day
/// ago" and "6 days ago" both mattering is the point.
const RELATIVE_FMT = new Intl.RelativeTimeFormat(undefined, { numeric: "always" });

/// Largest first, so the loop picks the coarsest unit that still has a
/// whole number in it. Month and year are the average lengths
/// `Intl.RelativeTimeFormat` itself assumes; at that distance the
/// column is saying "ages ago" and the hover carries the precision.
const RELATIVE_UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 31_557_600_000],
  ["month", 2_629_800_000],
  ["week", 604_800_000],
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
  ["second", 1000],
];

/// "5 minutes ago". `now` is passed in rather than sampled here, so a
/// caller rendering a whole column gets one consistent clock for it —
/// and so this is testable without freezing time.
export function formatRelative(iso: string | null, now: number): string {
  if (!iso) return "—";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return iso;
  const delta = now - t;
  // Under a second, "0 seconds ago" is worse than saying so plainly.
  // This also catches a stamp a moment in the *future*, which ordinary
  // clock skew between the machine that ran the sync and the machine
  // reading this page produces all the time.
  if (Math.abs(delta) < 1000) return "just now";
  for (const [unit, ms] of RELATIVE_UNITS) {
    if (Math.abs(delta) >= ms) {
      // Negated because `RelativeTimeFormat` counts forward: -5 minutes
      // is "5 minutes ago". A genuinely future stamp keeps its sign and
      // renders as "in 3 hours", which is the honest reading of a clock
      // that disagrees with ours by more than a moment.
      return RELATIVE_FMT.format(-Math.round(delta / ms), unit);
    }
  }
  return "just now";
}
