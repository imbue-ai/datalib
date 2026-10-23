// A date drawn as a chip (docs/dev/chips.md § "A date"): every
// `<time datetime>` a render writes, shown in the viewer's own zone by
// the one formatter here, with the precise instant on hover, a copy that
// keeps the instant, and a menu that searches around it. The rules are
// pure; `decorateDates` is the one function that touches the DOM.

import { formatRelative } from "@/config/timeFormat";
import { filterToken } from "@/grid/query";
import { drawChip, type ChipLook } from "./contacts";

/** What a `datetime` names: an instant with the offset it was written
 *  in, or a day with no time of day. */
export type Stamp =
  | { kind: "instant"; ms: number; offsetMin: number }
  | { kind: "day"; year: number; month: number; day: number };

const INSTANT =
  /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?(Z|([+-])(\d{2}):(\d{2}))$/;
const DAY = /^(\d{4})-(\d{2})-(\d{2})$/;

/** The stamp a `datetime` attribute names, or null for anything a render
 *  does not write: a time with no offset, a date that does not exist. */
export function parseStamp(datetime: string): Stamp | null {
  const s = datetime.trim();
  const i = INSTANT.exec(s);
  if (i) {
    const ms = Date.parse(s);
    if (Number.isNaN(ms)) return null;
    const offsetMin = i[7] === "Z" ? 0 : (i[8] === "-" ? -1 : 1) * (+i[9] * 60 + +i[10]);
    return { kind: "instant", ms, offsetMin };
  }
  const d = DAY.exec(s);
  if (!d) return null;
  const [year, month, day] = [+d[1], +d[2], +d[3]];
  const check = new Date(Date.UTC(year, month - 1, day));
  if (check.getUTCMonth() !== month - 1 || check.getUTCDate() !== day) return null;
  return { kind: "day", year, month, day };
}

/** A day as an instant at its noon in UTC, so formatting it in UTC can
 *  never cross into the day before or after. */
function dayMs(s: Extract<Stamp, { kind: "day" }>): number {
  return Date.UTC(s.year, s.month - 1, s.day, 12);
}

const p2 = (n: number) => String(n).padStart(2, "0");

/** `zone` is an IANA zone for tests; the viewer's own zone otherwise. */
function fmt(zone: string | undefined, opts: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  return new Intl.DateTimeFormat(undefined, { ...opts, timeZone: zone });
}

const SHORT_DAY: Intl.DateTimeFormatOptions = {
  weekday: "short",
  year: "numeric",
  month: "short",
  day: "numeric",
};
const LONG_DAY: Intl.DateTimeFormatOptions = {
  weekday: "long",
  year: "numeric",
  month: "long",
  day: "numeric",
};
const CLOCK: Intl.DateTimeFormatOptions = { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };

/** The text a date chip shows: `Tue, Apr 15, 2369, 12:05` in the
 *  viewer's zone, or `Tue, Mar 18, 2369` for a day. */
export function dateText(s: Stamp, zone?: string): string {
  if (s.kind === "day") return fmt("UTC", SHORT_DAY).format(dayMs(s));
  return fmt(zone, { ...SHORT_DAY, ...CLOCK }).format(s.ms);
}

/** The zone's short name at that instant, `GMT+2` or `PDT`. */
function zoneName(ms: number, zone: string | undefined): string {
  const parts = fmt(zone, { timeZoneName: "short" }).formatToParts(ms);
  return parts.find((p) => p.type === "timeZoneName")?.value ?? "";
}

/** The wall clock an offset reads at an instant: `2369-04-15 08:05:00`. */
function wallClock(ms: number, offsetMin: number): string {
  const d = new Date(ms + offsetMin * 60_000);
  return (
    `${d.getUTCFullYear()}-${p2(d.getUTCMonth() + 1)}-${p2(d.getUTCDate())} ` +
    `${p2(d.getUTCHours())}:${p2(d.getUTCMinutes())}:${p2(d.getUTCSeconds())}`
  );
}

function offsetText(min: number): string {
  const a = Math.abs(min);
  return `${min < 0 ? "-" : "+"}${p2(Math.floor(a / 60))}:${p2(a % 60)}`;
}

/** The viewer zone's offset from UTC at an instant, in minutes. */
function zoneOffsetMin(ms: number, zone: string | undefined): number {
  const parts = fmt(zone, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  }).formatToParts(ms);
  const n = (t: string) => +(parts.find((p) => p.type === t)?.value ?? 0);
  const asUtc = Date.UTC(n("year"), n("month") - 1, n("day"), n("hour"), n("minute"), n("second"));
  return Math.round((asUtc - Math.floor(ms / 1000) * 1000) / 60_000);
}

/** The hover: the whole instant to the second in the viewer's zone, the
 *  same in UTC, the clock the source wrote it in where that is neither,
 *  and how long ago. A day says which day and how long ago. */
export function dateTitle(s: Stamp, now: number, zone?: string): string {
  if (s.kind === "day") {
    const iso = `${s.year}-${p2(s.month)}-${p2(s.day)}`;
    const ago = formatRelative(new Date(dayMs(s)).toISOString(), now);
    return [fmt("UTC", LONG_DAY).format(dayMs(s)), `${iso} · ${ago}`].join("\n");
  }
  const here = fmt(zone, {
    ...LONG_DAY,
    ...CLOCK,
    second: "2-digit",
    timeZoneName: "short",
  }).format(s.ms);
  const lines = [here, `${wallClock(s.ms, 0)} UTC`];
  if (s.offsetMin !== 0 && s.offsetMin !== zoneOffsetMin(s.ms, zone)) {
    lines.push(`${wallClock(s.ms, s.offsetMin)} ${offsetText(s.offsetMin)} as written`);
  }
  lines.push(formatRelative(new Date(s.ms).toISOString(), now));
  return lines.join("\n");
}

/** How a date chip looks. `classes` keeps the element's own (`msg-ts`
 *  in a message header) through `drawChip`'s base class. */
export function dateLook(s: Stamp, now: number, zone?: string): ChipLook {
  const text = dateText(s, zone);
  return {
    text,
    ariaLabel: text,
    classes: ["handle-chip", "date-chip"],
    initial: null,
    icon: null,
    photo: null,
    title: dateTitle(s, now, zone),
  };
}

/** A date as copied text: what it shows, with the zone that reading is
 *  in, so a paste anywhere means the same instant. A day is its text. */
export function dateCopyText(s: Stamp, zone?: string): string {
  const text = dateText(s, zone);
  return s.kind === "day" ? text : `${text} ${zoneName(s.ms, zone)}`;
}

/** The viewer's day a stamp falls on, `YYYY-MM-DD`, and the day after. */
function daysAround(s: Stamp, zone: string | undefined): [string, string] {
  const ms = s.kind === "day" ? dayMs(s) : s.ms;
  const z = s.kind === "day" ? "UTC" : zone;
  const parts = fmt(z, { year: "numeric", month: "2-digit", day: "2-digit" }).formatToParts(ms);
  const n = (t: string) => +(parts.find((p) => p.type === t)?.value ?? 0);
  const next = new Date(Date.UTC(n("year"), n("month") - 1, n("day") + 1));
  return [
    `${n("year")}-${p2(n("month"))}-${p2(n("day"))}`,
    `${next.getUTCFullYear()}-${p2(next.getUTCMonth() + 1)}-${p2(next.getUTCDate())}`,
  ];
}

/** The search for everything on the stamp's day. A bare day in `after:`
 *  and `before:` is midnight on this machine, the viewer's zone. */
export function dayQuery(s: Stamp, zone?: string): string {
  const [day, next] = daysAround(s, zone);
  return `${filterToken("after", day, false)} ${filterToken("before", next, false)}`;
}

/** The search for everything before or after the stamp: the instant
 *  itself, or for a day, before it starts or after it ends. */
export function sideQuery(datetime: string, s: Stamp, side: "before" | "after"): string {
  if (s.kind === "instant") return filterToken(side, datetime.trim(), false);
  const [day, next] = daysAround(s, "UTC");
  return filterToken(side, side === "before" ? day : next, false);
}

/** A date chip for a surface that draws its own: a `<time>` element
 *  `decorateDates` would have made of the render's. */
export function dateCell(datetime: string, now = Date.now()): HTMLElement {
  const el = document.createElement("time");
  el.setAttribute("datetime", datetime);
  el.className = "chip";
  const s = parseStamp(datetime);
  if (s) drawChip(el, dateLook(s, now));
  else el.textContent = datetime;
  return el;
}

/** A copy from the document: each date chip becomes a plain `<time>`
 *  whose text is `dateCopyText`, so the plain text keeps the zone and
 *  the HTML keeps the instant. */
export function rewriteDatesForCopy(fragment: DocumentFragment | Element): boolean {
  const chips = Array.from(fragment.querySelectorAll<HTMLElement>("time.date-chip[datetime]"));
  for (const chip of chips) {
    const datetime = chip.getAttribute("datetime") ?? "";
    const s = parseStamp(datetime);
    if (!s) continue;
    const t = chip.ownerDocument.createElement("time");
    t.setAttribute("datetime", datetime);
    t.textContent = dateCopyText(s);
    chip.replaceWith(t);
  }
  return chips.length > 0;
}

export type DateMenuId = "copy-name" | "copy-id" | "search" | "before" | "after";
export type DateMenuEntry = { id: DateMenuId; label: string; separator?: boolean };

/** A date chip's menu: copy it as shown or as written, then search on
 *  its day, before it or after it. */
export function dateMenu(datetime: string, s: Stamp, zone?: string): DateMenuEntry[] {
  const day = s.kind === "day" ? dateText(s) : fmt(zone, SHORT_DAY).format(s.ms);
  return [
    { id: "copy-name", label: `Copy “${dateCopyText(s, zone)}”` },
    { id: "copy-id", label: `Copy ${datetime.trim()}` },
    { id: "search", label: `Everything on ${day}`, separator: true },
    { id: "before", label: "Everything before this" },
    { id: "after", label: "Everything after this" },
  ];
}

/** What the end of a span (`<span class="time-range">`, an event's
 *  start and end) shows when it falls on its start's day in the viewer's
 *  zone: its clock alone. Null when it shows its own day. */
export function rangeEndText(start: Stamp, end: Stamp, zone?: string): string | null {
  if (start.kind !== "instant" || end.kind !== "instant") return null;
  if (daysAround(start, zone)[0] !== daysAround(end, zone)[0]) return null;
  return fmt(zone, CLOCK).format(end.ms);
}

/** Draw every `<time datetime>` under `root` as a date chip. A stamp the
 *  viewer cannot read keeps the render's own text and hover. */
export function decorateDates(root: HTMLElement, now = Date.now(), zone?: string): void {
  for (const el of Array.from(root.querySelectorAll<HTMLElement>("time[datetime]"))) {
    const s = parseStamp(el.getAttribute("datetime") ?? "");
    if (!s) continue;
    el.classList.add("chip");
    el.draggable = true;
    const look = dateLook(s, now, zone);
    const text = spanEndText(el, s, zone);
    drawChip(el, text ? { ...look, text, ariaLabel: look.text } : look);
  }
}

/** The end of a span's clock-only text, where `el` is that end. */
function spanEndText(el: HTMLElement, end: Stamp, zone: string | undefined): string | null {
  const span = el.parentElement;
  if (!span?.classList.contains("time-range")) return null;
  const times = span.querySelectorAll<HTMLElement>(":scope > time[datetime]");
  if (times.length !== 2 || times[1] !== el) return null;
  const start = parseStamp(times[0]!.getAttribute("datetime") ?? "");
  return start ? rangeEndText(start, end, zone) : null;
}
