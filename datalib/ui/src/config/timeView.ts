// The zone and the format every timestamp in the app is drawn in: the
// choice the status bar's clock sets, kept per browser. `timeFormat.ts`
// and `cards/dates.ts` draw through the rules here; this file holds the
// setting and the pure functions that read an instant under it.
import { ref, watch } from "vue";

/** Where an instant is read: this machine's zone, the offset each stamp
 *  was written in, or one named IANA zone (`UTC` among them). */
export type Zone = { kind: "local" } | { kind: "written" } | { kind: "named"; name: string };

/** A day as `Wed, Apr 15, 2026` in the viewer's language, or `2026-04-15`. */
export type DayStyle = "locale" | "iso";

export type HourCycle = "h23" | "h12";

/** `t0`, when set, is the instant every date chip is drawn relative to
 *  (`sinceT0`) instead of as a date. */
export type TimeView = { zone: Zone; days: DayStyle; hours: HourCycle; t0?: Instant | null };

export const LOCAL: Zone = { kind: "local" };
export const WRITTEN: Zone = { kind: "written" };
export const UTC: Zone = { kind: "named", name: "UTC" };
export const DEFAULT_VIEW: TimeView = { zone: LOCAL, days: "locale", hours: "h23" };

/** An instant and the offset it was written in, where it names one. */
export type Instant = { ms: number; offsetMin: number | null };

const OFFSET = /(Z|([+-])(\d{2}):?(\d{2}))$/;

/** The instant an ISO stamp names and its offset; null when it is not one. */
export function parseInstant(iso: string): Instant | null {
  const s = iso.trim();
  const ms = Date.parse(s);
  if (Number.isNaN(ms)) return null;
  const m = /T\d/.test(s) ? OFFSET.exec(s) : null;
  if (!m) return { ms, offsetMin: null };
  const offsetMin = m[1] === "Z" ? 0 : (m[2] === "-" ? -1 : 1) * (+m[3]! * 60 + +m[4]!);
  return { ms, offsetMin };
}

export function isValidZone(name: string): boolean {
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: name });
    return true;
  } catch {
    return false;
  }
}

const formats = new Map<string, Intl.DateTimeFormat>();

/** One formatter per zone and options: making one costs far more than
 *  using it, and a grid formats thousands. */
function fmt(tz: string | undefined, opts: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  const key = `${tz ?? ""}|${JSON.stringify(opts)}`;
  let f = formats.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(undefined, { ...opts, timeZone: tz });
    formats.set(key, f);
  }
  return f;
}

/** How an instant is read under `zone`: an Intl zone (undefined is this
 *  machine's) and a shift. "As written" reads the instant moved by its
 *  own offset in UTC, which every engine formats; a stamp with no offset
 *  of its own is read on this machine's clock. */
function reading(zone: Zone, at: Instant): { tz: string | undefined; ms: number } {
  if (zone.kind === "named") return { tz: zone.name, ms: at.ms };
  if (zone.kind === "written" && at.offsetMin !== null) {
    return { tz: "UTC", ms: at.ms + at.offsetMin * 60_000 };
  }
  return { tz: undefined, ms: at.ms };
}

/** The wall clock an instant reads under `zone`. */
export type Wall = {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
  second: number;
};

const WALL_OPTS: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
};

export function wallClock(at: Instant, zone: Zone): Wall {
  const r = reading(zone, at);
  const parts = fmt(r.tz, WALL_OPTS).formatToParts(r.ms);
  const n = (t: Intl.DateTimeFormatPartTypes) => +(parts.find((p) => p.type === t)?.value ?? 0);
  return {
    year: n("year"),
    month: n("month"),
    day: n("day"),
    hour: n("hour"),
    minute: n("minute"),
    second: n("second"),
  };
}

/** The zone's offset from UTC at an instant, in minutes. */
export function offsetAt(at: Instant, zone: Zone): number {
  const w = wallClock(at, zone);
  const asUtc = Date.UTC(w.year, w.month - 1, w.day, w.hour, w.minute, w.second);
  return Math.round((asUtc - Math.floor(at.ms / 1000) * 1000) / 60_000);
}

const p2 = (n: number) => String(n).padStart(2, "0");

export function offsetText(min: number): string {
  const a = Math.abs(min);
  return `${min < 0 ? "-" : "+"}${p2(Math.floor(a / 60))}:${p2(a % 60)}`;
}

/** The zone's short name at an instant, `GMT+2` or `PDT`; for a stamp
 *  read as written, its offset in the same shape. */
export function zoneName(at: Instant, zone: Zone): string {
  const r = reading(zone, at);
  if (zone.kind === "written" && at.offsetMin !== null) {
    const m = at.offsetMin;
    if (m === 0) return "UTC";
    const a = Math.abs(m);
    return `GMT${m < 0 ? "-" : "+"}${Math.floor(a / 60)}${a % 60 ? `:${p2(a % 60)}` : ""}`;
  }
  const parts = fmt(r.tz, { timeZoneName: "short" }).formatToParts(r.ms);
  return parts.find((p) => p.type === "timeZoneName")?.value ?? "";
}

/** `Wed, Apr 15, 2026`, `Apr 15, 2026`, `Wednesday, April 15, 2026`. */
export type DayWidth = "short" | "medium" | "long";

const DAY_OPTS: Record<DayWidth, Intl.DateTimeFormatOptions> = {
  short: { weekday: "short", year: "numeric", month: "short", day: "numeric" },
  medium: { year: "numeric", month: "short", day: "numeric" },
  long: { weekday: "long", year: "numeric", month: "long", day: "numeric" },
};

/** What `formatAt` writes: the day, the clock, or both; with seconds or
 *  the year left off where the caller has no room for them. */
export type Shape = {
  day?: DayWidth | "noYear" | "monthDay";
  clock?: boolean;
  seconds?: boolean;
  zoneName?: boolean;
};

function clockText(w: Wall, hours: HourCycle, seconds: boolean): string {
  const s = seconds ? `:${p2(w.second)}` : "";
  if (hours === "h23") return `${p2(w.hour)}:${p2(w.minute)}${s}`;
  const h = w.hour % 12 || 12;
  return `${h}:${p2(w.minute)}${s} ${w.hour < 12 ? "AM" : "PM"}`;
}

function isoDay(w: Wall, shape: Shape["day"]): string {
  if (shape === "noYear" || shape === "monthDay") return `${p2(w.month)}-${p2(w.day)}`;
  return `${w.year}-${p2(w.month)}-${p2(w.day)}`;
}

function localeOpts(shape: Shape, hours: HourCycle): Intl.DateTimeFormatOptions {
  const day: Intl.DateTimeFormatOptions =
    shape.day === "noYear"
      ? { weekday: "short", month: "short", day: "numeric" }
      : shape.day === "monthDay"
        ? { month: "short", day: "numeric" }
        : shape.day
          ? DAY_OPTS[shape.day]
          : {};
  const clock: Intl.DateTimeFormatOptions = shape.clock
    ? {
        hour: hours === "h23" ? "2-digit" : "numeric",
        minute: "2-digit",
        ...(shape.seconds ? { second: "2-digit" } : {}),
        hourCycle: hours,
      }
    : {};
  return { ...day, ...clock };
}

/** An instant as text under a view: `Wed, Apr 15, 2026, 12:05` or
 *  `2026-04-15 12:05`, as much of it as `shape` asks for. */
export function formatAt(at: Instant, view: TimeView, shape: Shape): string {
  let text: string;
  if (view.days === "iso") {
    const w = wallClock(at, view.zone);
    const parts = [
      shape.day ? isoDay(w, shape.day) : "",
      shape.clock ? clockText(w, view.hours, !!shape.seconds) : "",
    ];
    text = parts.filter(Boolean).join(" ");
  } else {
    const r = reading(view.zone, at);
    text = fmt(r.tz, localeOpts(shape, view.hours)).format(r.ms);
  }
  return shape.zoneName ? `${text} ${zoneName(at, view.zone)}` : text;
}

/** A calendar day, which is the same day wherever it is read: only the
 *  style applies. */
export function formatDay(
  year: number,
  month: number,
  day: number,
  view: TimeView,
  width: DayWidth,
): string {
  if (view.days === "iso") return `${year}-${p2(month)}-${p2(day)}`;
  return fmt("UTC", DAY_OPTS[width]).format(Date.UTC(year, month - 1, day, 12));
}

/** The instant a wall clock under `zone` names; for a clock a DST change
 *  skips, the one an hour on. `offsetMin` is the stamp's own, for "as
 *  written". */
export function instantOfWall(w: Wall, zone: Zone, offsetMin: number | null): number {
  const asUtc = Date.UTC(w.year, w.month - 1, w.day, w.hour, w.minute, w.second);
  let guess = asUtc;
  for (let i = 0; i < 2; i++) {
    guess = asUtc - offsetAt({ ms: guess, offsetMin }, zone) * 60_000;
  }
  return guess;
}

/** Midnight that starts the day an instant falls on under `zone`, and
 *  the one that ends it, each as an ISO stamp with that zone's offset at
 *  that moment: a search names exactly that day, wherever it runs. */
export function dayBounds(at: Instant, zone: Zone): [string, string] {
  const w = wallClock(at, zone);
  const next = new Date(Date.UTC(w.year, w.month - 1, w.day + 1));
  const start = { year: w.year, month: w.month, day: w.day, hour: 0, minute: 0, second: 0 };
  const end = {
    year: next.getUTCFullYear(),
    month: next.getUTCMonth() + 1,
    day: next.getUTCDate(),
    hour: 0,
    minute: 0,
    second: 0,
  };
  const stamp = (wall: Wall) => {
    const ms = instantOfWall(wall, zone, at.offsetMin);
    const off = offsetAt({ ms, offsetMin: at.offsetMin }, zone);
    const local = wallClock({ ms, offsetMin: at.offsetMin }, zone);
    return (
      `${local.year}-${p2(local.month)}-${p2(local.day)}T` +
      `${p2(local.hour)}:${p2(local.minute)}:${p2(local.second)}${offsetText(off)}`
    );
  };
  return [stamp(start), stamp(end)];
}

/** An instant to the millisecond under `zone`, `2026-09-22 14:07:47.190`,
 *  for a log: a step's own lines are stamped in UTC and the runner's in
 *  local time, and showing the digits as written would interleave two
 *  clocks. Digits and separators only, on a 24-hour clock whatever the
 *  view's, so it sorts as text and a column can clip it from the left
 *  down to the time of day. */
export function millisText(at: Instant, zone: Zone): string {
  const w = wallClock(at, zone);
  const ms = String(((at.ms % 1000) + 1000) % 1000).padStart(3, "0");
  return `${w.year}-${p2(w.month)}-${p2(w.day)} ${p2(w.hour)}:${p2(w.minute)}:${p2(w.second)}.${ms}`;
}

/** How far `ms` is from T0, signed, to the millisecond: `+1.250`,
 *  `-4:05.000`, `+2:00:00.000`, `-3d 04:05:06.789`, `+343y 075d
 *  00:23:24.454`. The largest unit that is not zero leads, unpadded; the
 *  rest are padded, so a column of them lines up when right-justified.
 *  A year is a calendar year counted on from the earlier of the two in
 *  UTC, so the same day a year on is `1y 000d`, leap day or not. */
export function sinceT0(ms: number, t0: number): string {
  const { sign, years, days, hours, minutes, seconds, millis } = partsSinceT0(ms, t0);
  const frac = `.${String(millis).padStart(3, "0")}`;
  const clock = `${p2(hours)}:${p2(minutes)}:${p2(seconds)}`;
  const whole = years
    ? `${years}y ${String(days).padStart(3, "0")}d ${clock}`
    : days
      ? `${days}d ${clock}`
      : hours
        ? `${hours}:${p2(minutes)}:${p2(seconds)}`
        : minutes
          ? `${minutes}:${p2(seconds)}`
          : `${seconds}`;
  return `${sign < 0 ? "-" : sign > 0 ? "+" : ""}${whole}${frac}`;
}

/** How far `ms` is from T0, in its units: calendar years, then days down
 *  to milliseconds, and which side of T0 it is on. */
export type SinceT0 = {
  sign: -1 | 0 | 1;
  years: number;
  days: number;
  hours: number;
  minutes: number;
  seconds: number;
  millis: number;
};

export function partsSinceT0(ms: number, t0: number): SinceT0 {
  const d = Math.round(ms - t0);
  const [from, to] = d < 0 ? [t0 + d, t0] : [t0, t0 + d];
  const years = wholeYears(from, to);
  const a = to - yearsOn(from, years);
  return {
    sign: d < 0 ? -1 : d > 0 ? 1 : 0,
    years,
    days: Math.floor(a / 86_400_000),
    hours: Math.floor(a / 3_600_000) % 24,
    minutes: Math.floor(a / 60_000) % 60,
    seconds: Math.floor(a / 1000) % 60,
    millis: a % 1000,
  };
}

/** The same instant `years` calendar years on (or back), in UTC. */
export function yearsOn(ms: number, years: number): number {
  const at = new Date(ms);
  at.setUTCFullYear(at.getUTCFullYear() + years);
  return at.getTime();
}

/** The calendar years from `from` that fit before `to`. */
function wholeYears(from: number, to: number): number {
  let years = Math.floor((to - from) / 31_556_952_000);
  while (years > 0 && yearsOn(from, years) > to) years--;
  while (yearsOn(from, years + 1) <= to) years++;
  return years;
}

/** Whether a view reads every instant as this machine does, which is how
 *  the search reads a bare day. */
export function readsAsMachine(view: TimeView): boolean {
  return view.zone.kind === "local";
}

/** A named zone as a person reads it: `Vienna`, `Los Angeles`, `UTC`. */
export function zonePlace(name: string): string {
  return (name.split("/").at(-1) ?? name).replace(/_/g, " ");
}

/** The named zones `query` finds, by name (`kolk`, `los angeles`) or by
 *  their offset now (`+05:30`, `utc-7`), each with that offset. At most
 *  `limit`. */
export function searchZones(
  query: string,
  zones: readonly string[],
  now: number,
  limit = 12,
): { name: string; offset: string }[] {
  const q = query.trim().toLowerCase().replace(/\s+/g, "_");
  if (!q) return [];
  // An offset is asked for by its sign; anything else is a name, and
  // only a name that matches costs a formatter.
  const byOffset = /^(utc|gmt)?[+-]/.test(q)
    ? q.replace(/^(utc|gmt)/, "").replace(/^([+-])(\d)(?=$|:)/, (_, sign, h) => `${sign}0${h}`)
    : null;
  const out: { name: string; offset: string }[] = [];
  for (const name of zones) {
    if (byOffset === null && !name.toLowerCase().includes(q)) continue;
    const offset = offsetText(offsetAt({ ms: now, offsetMin: null }, { kind: "named", name }));
    if (byOffset !== null && !offset.startsWith(byOffset)) continue;
    out.push({ name, offset });
    if (out.length >= limit) break;
  }
  return out;
}

export function allZones(): readonly string[] {
  return Intl.supportedValuesOf?.("timeZone") ?? [];
}

/** The most recent first, `zone` added, at most `max`; only named zones
 *  other than UTC, which has its own entry. */
export function withRecent(recent: readonly string[], zone: Zone, max = 5): string[] {
  if (zone.kind !== "named" || zone.name === "UTC") return [...recent];
  return [zone.name, ...recent.filter((z) => z !== zone.name)].slice(0, max);
}

/** A stored view, or the default for anything this build cannot read:
 *  a zone this engine does not know reads as local. */
export function readStoredView(raw: string | null): TimeView {
  try {
    const v = JSON.parse(raw ?? "") as Partial<TimeView>;
    const z = v.zone;
    const zone: Zone =
      z?.kind === "written"
        ? WRITTEN
        : z?.kind === "named" && typeof z.name === "string" && isValidZone(z.name)
          ? { kind: "named", name: z.name }
          : LOCAL;
    const t0 = v.t0;
    return {
      zone,
      days: v.days === "iso" ? "iso" : "locale",
      hours: v.hours === "h12" ? "h12" : "h23",
      ...(t0 && Number.isFinite(t0.ms) && (t0.offsetMin === null || Number.isFinite(t0.offsetMin))
        ? { t0: { ms: t0.ms, offsetMin: t0.offsetMin } }
        : {}),
    };
  } catch {
    return DEFAULT_VIEW;
  }
}

export function readStoredRecent(raw: string | null): string[] {
  try {
    const v = JSON.parse(raw ?? "[]") as unknown;
    return Array.isArray(v)
      ? v.filter((z): z is string => typeof z === "string" && isValidZone(z))
      : [];
  } catch {
    return [];
  }
}

const VIEW_KEY = "datalib-time-view";
const RECENT_KEY = "datalib-time-zones-recent";

function load(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function save(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Blocked storage: the choice lasts as long as the page.
  }
}

/** The view every timestamp is drawn in now. A template that formats a
 *  stamp reads it and so redraws when it changes; code that draws into
 *  the DOM itself watches it. */
export const timeView = ref<TimeView>(readStoredView(load(VIEW_KEY)));
export const recentZones = ref<string[]>(readStoredRecent(load(RECENT_KEY)));

export function setZone(zone: Zone) {
  timeView.value = { ...timeView.value, zone };
  recentZones.value = withRecent(recentZones.value, zone);
}

/** Draw every date chip relative to `at`, or as a date again for null. */
export function setT0(at: Instant | null) {
  timeView.value = { ...timeView.value, t0: at };
}

watch(timeView, (v) => save(VIEW_KEY, v), { deep: true });
watch(recentZones, (v) => save(RECENT_KEY, v));
