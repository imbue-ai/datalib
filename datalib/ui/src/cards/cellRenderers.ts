// The cell renderers the column-type vocabulary draws with: a byte
// count as a size, an identity as its icon and label, a status as its
// glyph, a sparkline for a time series. Plain DOM, owned by no grid;
// `typedColumns.ts` maps a `ColumnSpec` onto them. Nothing here knows
// what the rows are.
import type { Chip, ColumnSpec, ColumnType, Identity, StatusView, Timeseries } from "@/api";
import { iconUrl } from "@/config/icons";
import { STATUS_GLYPHS, STEP_GLYPHS, glyphSvg } from "@/config/glyphs";
import { ownRange, sparkline, windowDelta, type Sample } from "@/config/sparkline";
import { formatRelative, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";

/// The fields of one type among the specs. A `timestamp` cell reads "5
/// minutes ago", so it goes stale on its own and the host repaints it
/// on a clock.
export function fieldsOfType(specs: ColumnSpec[], type: ColumnType): string[] {
  return specs.filter((c) => c.type === type).map((c) => c.field);
}

// ── Icon tokens ──────────────────────────────────────────────────
// A producer names an icon as a token; this is where the token becomes
// a picture. Brand marks are bundled assets; `step:<phase>` and
// `applet` are the pipeline-role glyphs.

function iconFor(token: string | null | undefined, label: string): Element | null {
  if (!token) return null;
  const url = iconUrl(token);
  if (url) {
    const img = document.createElement("img");
    img.src = url;
    img.alt = label;
    img.title = label;
    img.className = "tg-brand";
    return img;
  }
  const glyph =
    token === "applet"
      ? STEP_GLYPHS.applet
      : token.startsWith("step:")
        ? STEP_GLYPHS[token.slice(5) as keyof typeof STEP_GLYPHS]
        : undefined;
  return glyph ? glyphSvg(glyph, label, 14) : null;
}

function formatUnit(n: number, unit: string): string {
  return unit === "bytes" ? formatBytes(n) : n.toLocaleString();
}

export function none(): HTMLElement {
  const span = document.createElement("span");
  span.className = "tg-none";
  span.textContent = "—";
  return span;
}

function windowPhrase(secs: number): string {
  return secs % 60 === 0
    ? `the last ${secs / 60} minute${secs === 60 ? "" : "s"}`
    : `the last ${secs} seconds`;
}

const SPARK = { width: 120, height: 18 };

function sparkSvg(value: number, samples: Sample[], windowMs: number): SVGSVGElement | null {
  const nowMs = Date.now();
  const spark = sparkline(samples, {
    nowMs,
    windowMs,
    ...ownRange(value, samples, nowMs, windowMs),
    width: SPARK.width,
    height: SPARK.height,
    inset: 0.5,
  });
  if (!spark) return null;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${SPARK.width} ${SPARK.height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("tg-spark");
  const area = document.createElementNS("http://www.w3.org/2000/svg", "polygon");
  area.setAttribute("points", spark.area);
  area.classList.add("tg-spark-area");
  svg.appendChild(area);
  const line = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
  line.setAttribute("points", spark.line);
  line.classList.add("tg-spark-line");
  svg.appendChild(line);
  return svg;
}

/// A series drawn as a sparkline with its present value and its change
/// over the window laid over it. The Manage table's cells and the
/// status bar's data-root total are both this, so they read alike.
/// `change` says the movement in words, for the hover.
export function sparkTrack(
  value: number,
  unit: string,
  samples: Sample[],
  windowSecs: number,
): { el: HTMLElement; change: string } {
  const windowMs = windowSecs * 1000;
  const track = document.createElement("span");
  track.className = "tg-plot";
  // No samples yet means the producer hasn't measured twice. The value
  // still shows; there is just nothing behind it to draw.
  const svg = sparkSvg(value, samples, windowMs);
  if (svg) track.appendChild(svg);
  const label = document.createElement("span");
  label.className = "tg-plot-label";
  const now = document.createElement("span");
  now.className = "tg-plot-value";
  now.textContent = formatUnit(value, unit);
  label.appendChild(now);
  const moved = windowDelta(value, samples, Date.now(), windowMs);
  if (moved) {
    const delta = document.createElement("span");
    delta.className = "tg-plot-delta";
    delta.textContent = `${moved > 0 ? "+" : "−"}${formatUnit(Math.abs(moved), unit)}`;
    label.appendChild(delta);
  }
  track.appendChild(label);
  const change = moved
    ? // Said as a change rather than as two endpoints: both endpoints
      // round to the same figure whenever the movement is small
      // against the total.
      `${moved > 0 ? "Grew" : "Shrank"} by ${formatUnit(Math.abs(moved), unit)} over ` +
      `${windowPhrase(windowSecs)}. The line is scaled to its own range rather than to ` +
      `zero, so its height is the shape of the change, not the size.`
    : `No change over ${windowPhrase(windowSecs)}.`;
  return { el: track, change };
}

// ── Cell renderers, one per type ─────────────────────────────────

export function renderIdentity(
  v: Identity | null | undefined,
  isTreeColumn: boolean,
  isParent: boolean,
): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-identity";
  if (!v) return wrap;
  const icon = iconFor(v.icon, v.detail ?? v.label);
  // A brand mark leads; a role glyph follows the name, muted, because
  // the name is what the eye should land on and the glyph answers the
  // follow-up question.
  const brand = icon?.tagName === "IMG";
  if (icon && brand) wrap.appendChild(icon);
  const text = document.createElement("span");
  text.textContent = v.label;
  if (isParent) text.className = "tg-parent";
  text.title = v.detail ? `${v.id} — ${v.detail}` : v.id;
  wrap.appendChild(text);
  if (icon && !brand) {
    const mark = document.createElement("span");
    mark.className = "tg-mark";
    mark.title = v.detail ?? "";
    mark.appendChild(icon);
    wrap.appendChild(mark);
  }
  // The row's own id, where the column is the row's identity and the
  // label hides it.
  if (isTreeColumn && v.id !== v.label) {
    const id = document.createElement("span");
    id.className = "tg-id";
    id.textContent = v.id;
    id.title = `Id — stored in ${v.id}/ under the data root`;
    wrap.appendChild(id);
  }
  return wrap;
}

export function renderStatus(s: StatusView | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  if (!s) return wrap;
  const { key, label } = s;
  wrap.className = `tg-status tg-status-${key.replace(/[\s_]+/g, "-")}`;
  // The word, and then why it is that word. The column is glyphs, so
  // this is the only place either appears.
  wrap.title = s.detail ? `${label} — ${s.detail}` : label;
  const spinnerOrGlyph = () => {
    if (key === "running") {
      // A still frame can't say "still going", so running is the one
      // state drawn rather than glyphed.
      const spin = document.createElement("span");
      spin.className = "tg-spinner";
      spin.setAttribute("role", "img");
      spin.setAttribute("aria-label", label);
      return spin;
    }
    const glyph = STATUS_GLYPHS[key];
    if (glyph) return glyphSvg(glyph, label);
    // A status this sheet hasn't met. Say the word rather than draw
    // nothing — an unknown state is exactly when a reader most needs
    // to know what it was.
    const word = document.createElement("span");
    word.textContent = label;
    word.setAttribute("role", "img");
    word.setAttribute("aria-label", label);
    return word;
  };
  wrap.appendChild(spinnerOrGlyph());
  if (s.segments) {
    // One segment per part, each in its own status colour, the running
    // one pulsing. No arithmetic across parts; the bar *is* the parts.
    const bar = document.createElement("span");
    bar.className = "tg-segs";
    for (const seg of s.segments) {
      const cell = document.createElement("span");
      cell.className = `tg-seg tg-seg-${seg.key.replace(/[\s_]+/g, "-")}`;
      cell.title = `${seg.id}: ${seg.label}`;
      bar.appendChild(cell);
    }
    wrap.appendChild(bar);
  } else if (key === "running" && s.fraction != null) {
    // A bar only when the thing said how much is ahead of it: a bar at
    // an invented fraction claims more than we know.
    const bar = document.createElement("span");
    bar.className = "tg-progress";
    const fill = document.createElement("span");
    fill.style.width = `${s.fraction * 100}%`;
    bar.appendChild(fill);
    wrap.appendChild(bar);
  }
  return wrap;
}

export function renderChips(chips: Chip[] | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-chips";
  if (!chips?.length) return wrap;
  // The chips are cut at the column's edge; the whole row of them is
  // one hover away.
  wrap.title = chips.map((c) => c.text).join("  ");
  for (const chip of chips) {
    const el = document.createElement("span");
    el.className = `tg-chip tg-chip-${chip.kind}`;
    el.textContent = chip.text;
    el.title = chip.title;
    wrap.appendChild(el);
  }
  return wrap;
}

export function renderTimestamp(iso: string | null | undefined): HTMLElement {
  if (!iso) return none();
  const span = document.createElement("span");
  span.textContent = formatRelative(iso, Date.now());
  // The exact stamp, for when "7 days ago" isn't the answer you needed.
  span.title = formatStamp(iso);
  return span;
}

export function renderTimeseries(
  v: Timeseries | null | undefined,
  windowSecs: number,
): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-series";
  if (!v || v.value === null) {
    // Null is "nothing measured yet", which is not a flat line at zero
    // — it's the absence of a plot.
    wrap.appendChild(none());
    if (v?.detail) wrap.title = v.detail;
    return wrap;
  }
  const track = sparkTrack(v.value, v.unit, v.samples, windowSecs);
  wrap.title = `${v.detail ?? formatUnit(v.value, v.unit)}\n${track.change}`;
  wrap.appendChild(track.el);
  return wrap;
}

export const WIDTH: Record<ColumnType, number> = {
  text: 150,
  count: 90,
  number: 90,
  bytes: 110,
  timestamp: 150,
  datetime: 165,
  timeseries: 170,
  identity: 120,
  status: 96,
  chips: 260,
  actions: 92,
  markdown_uuid: 200,
};
