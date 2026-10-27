// A date drawn as a chip (docs/dev/chips.md § "A date"): every
// `<time datetime>` a render writes, shown in the zone and format the
// status bar's clock sets (`config/timeView.ts`) by the one formatter here, with the precise instant on hover, a copy that
// keeps the instant, and a menu that searches around it. The rules are
// pure; `decorateDates` is the one function that touches the DOM.

import { formatRelative } from "@/config/timeFormat";
import {
  LOCAL,
  UTC,
  WRITTEN,
  dayBounds,
  formatAt,
  formatDay,
  offsetAt,
  offsetText,
  readsAsMachine,
  timeView,
  wallClock,
  zoneName,
  type TimeView,
  type Wall,
} from "@/config/timeView";
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

function wallText(w: Wall): string {
  return `${w.year}-${p2(w.month)}-${p2(w.day)} ` + `${p2(w.hour)}:${p2(w.minute)}:${p2(w.second)}`;
}

/** The text a date chip shows: `Tue, Apr 15, 2369, 12:05` in the
 *  view's zone, or `Tue, Mar 18, 2369` for a day. */
export function dateText(s: Stamp, view = timeView.value): string {
  if (s.kind === "day") return formatDay(s.year, s.month, s.day, view, "short");
  return formatAt(s, view, { day: "short", clock: true });
}

/** The hover: the whole instant to the second in the view's zone, the
 *  same in UTC, the clock the source wrote it in where that is neither,
 *  and how long ago. A day says which day and how long ago. */
export function dateTitle(s: Stamp, now: number, view = timeView.value): string {
  if (s.kind === "day") {
    const iso = `${s.year}-${p2(s.month)}-${p2(s.day)}`;
    const ago = formatRelative(new Date(dayMs(s)).toISOString(), now);
    return [
      formatDay(s.year, s.month, s.day, { ...view, days: "locale" }, "long"),
      `${iso} · ${ago}`,
    ].join("\n");
  }
  const here = formatAt(
    s,
    { ...view, days: "locale" },
    {
      day: "long",
      clock: true,
      seconds: true,
      zoneName: true,
    },
  );
  const lines = [here, `${wallText(wallClock(s, UTC))} UTC`];
  const shown = offsetAt(s, view.zone);
  if (s.offsetMin !== 0 && s.offsetMin !== shown) {
    lines.push(`${wallText(wallClock(s, WRITTEN))} ${offsetText(s.offsetMin)} as written`);
  }
  lines.push(formatRelative(new Date(s.ms).toISOString(), now));
  return lines.join("\n");
}

/** How a date chip looks. `classes` keeps the element's own (`msg-ts`
 *  in a message header) through `drawChip`'s base class. */
export function dateLook(s: Stamp, now: number, view = timeView.value): ChipLook {
  const text = dateText(s, view);
  return {
    text,
    ariaLabel: text,
    classes: ["handle-chip", "date-chip"],
    initial: null,
    icon: null,
    photo: null,
    title: dateTitle(s, now, view),
  };
}

/** A date as copied text: what it shows, with the zone that reading is
 *  in, so a paste anywhere means the same instant. A day is its text. */
export function dateCopyText(s: Stamp, view = timeView.value): string {
  const text = dateText(s, view);
  return s.kind === "day" ? text : `${text} ${zoneName(s, view.zone)}`;
}

/** A calendar day, `YYYY-MM-DD`, and the day after. */
function dayAndNext(year: number, month: number, day: number): [string, string] {
  const next = new Date(Date.UTC(year, month - 1, day + 1));
  return [
    `${year}-${p2(month)}-${p2(day)}`,
    `${next.getUTCFullYear()}-${p2(next.getUTCMonth() + 1)}-${p2(next.getUTCDate())}`,
  ];
}

/** The search for everything on the stamp's day in the view's zone. The
 *  search reads a bare day in `after:` and `before:` as midnight on the
 *  machine that runs it, so a day there is written bare only when the
 *  view reads instants as this machine does; otherwise its two midnights
 *  are written out with their offsets. */
export function dayQuery(s: Stamp, view = timeView.value): string {
  const [from, to] = dayRange(s, view);
  return `${filterToken("after", from, false)} ${filterToken("before", to, false)}`;
}

function dayRange(s: Stamp, view: TimeView): [string, string] {
  if (s.kind === "day") return dayAndNext(s.year, s.month, s.day);
  if (!readsAsMachine(view)) return dayBounds(s, view.zone);
  const w = wallClock(s, LOCAL);
  return dayAndNext(w.year, w.month, w.day);
}

/** The search for everything before or after the stamp: the instant
 *  itself, or for a day, before it starts or after it ends. */
export function sideQuery(datetime: string, s: Stamp, side: "before" | "after"): string {
  if (s.kind === "instant") return filterToken(side, datetime.trim(), false);
  const [day, next] = dayAndNext(s.year, s.month, s.day);
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
export function dateMenu(datetime: string, s: Stamp, view = timeView.value): DateMenuEntry[] {
  const day = s.kind === "day" ? dateText(s, view) : formatAt(s, view, { day: "short" });
  return [
    { id: "copy-name", label: `Copy “${dateCopyText(s, view)}”` },
    { id: "copy-id", label: `Copy ${datetime.trim()}` },
    { id: "search", label: `Everything on ${day}`, separator: true },
    { id: "before", label: "Everything before this" },
    { id: "after", label: "Everything after this" },
  ];
}

/** What the end of a span (`<span class="time-range">`, an event's
 *  start and end) shows when it falls on its start's day in the view's
 *  zone: its clock alone. Null when it shows its own day. */
export function rangeEndText(start: Stamp, end: Stamp, view = timeView.value): string | null {
  if (start.kind !== "instant" || end.kind !== "instant") return null;
  const [a, b] = [wallClock(start, view.zone), wallClock(end, view.zone)];
  if (a.year !== b.year || a.month !== b.month || a.day !== b.day) return null;
  return formatAt(end, view, { clock: true });
}

/** Draw every `<time datetime>` under `root` as a date chip. A stamp the
 *  viewer cannot read keeps the render's own text and hover. */
export function decorateDates(root: HTMLElement, now = Date.now(), view = timeView.value): void {
  for (const el of Array.from(root.querySelectorAll<HTMLElement>("time[datetime]"))) {
    const s = parseStamp(el.getAttribute("datetime") ?? "");
    if (!s) continue;
    el.classList.add("chip");
    el.draggable = true;
    const look = dateLook(s, now, view);
    const text = spanEndText(el, s, view);
    drawChip(el, text ? { ...look, text, ariaLabel: look.text } : look);
  }
}

/** The end of a span's clock-only text, where `el` is that end. */
function spanEndText(el: HTMLElement, end: Stamp, view: TimeView): string | null {
  const span = el.parentElement;
  if (!span?.classList.contains("time-range")) return null;
  const times = span.querySelectorAll<HTMLElement>(":scope > time[datetime]");
  if (times.length !== 2 || times[1] !== el) return null;
  const start = parseStamp(times[0]!.getAttribute("datetime") ?? "");
  return start ? rangeEndText(start, end, view) : null;
}
