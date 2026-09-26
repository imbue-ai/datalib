// The map card's decisions, as pure functions over values: what a point
// is coloured by, which categories get a colour, where the view sits,
// which point is under the pointer, and what a preview says. The card
// (UmapCard.ce.vue) draws and listens; everything it decides is here.

import type { MapPoint } from "@/api";

export type ColorBy = "provider" | "source" | "kind" | "year" | "account";

export const COLOR_BY: { key: ColorBy; label: string }[] = [
  { key: "provider", label: "Type" },
  { key: "source", label: "Source" },
  { key: "kind", label: "Kind" },
  { key: "year", label: "Year" },
  { key: "account", label: "Account" },
];

export function parseColorBy(s: string | null | undefined): ColorBy {
  return COLOR_BY.some((c) => c.key === s) ? (s as ColorBy) : "provider";
}

export const OTHER = "Other";
const BLANK = "(none)";

export function categoryOf(p: MapPoint, by: ColorBy): string {
  const v =
    by === "year" ? (p.created_at?.slice(0, 4) ?? "") : by === "provider" ? p.provider : p[by];
  return v || BLANK;
}

/// The eight categorical slots, in the order that keeps neighbours
/// apart under colour-blindness (validated with the dataviz skill's
/// script against the app's own surfaces). A ninth category is never a
/// generated hue: past seven, the rest share `Other`, drawn in grey.
export const SLOTS = {
  light: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#4a3aa7", "#e34948"],
  dark: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
};
export const OTHER_COLOR = { light: "#9a9892", dark: "#77766f" };
const NAMED = SLOTS.light.length - 1;

export type LegendEntry = { key: string; count: number; slot: number | null };

/// Which categories get their own colour: the most common, in order of
/// size, the rest folded into `Other`. Counted over the whole map, never
/// over what a filter left, so narrowing the map repaints nothing.
export function legendFor(points: MapPoint[], by: ColorBy): LegendEntry[] {
  const counts = new Map<string, number>();
  for (const p of points) {
    const k = categoryOf(p, by);
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  const ranked = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  // Years read in their own order, not by size.
  if (by === "year") ranked.sort((a, b) => b[0].localeCompare(a[0]));
  if (ranked.length <= NAMED + 1) {
    return ranked.map(([key, count], slot) => ({ key, count, slot }));
  }
  const named = ranked.slice(0, NAMED).map(([key, count], slot) => ({ key, count, slot }));
  const rest = ranked.slice(NAMED).reduce((n, [, c]) => n + c, 0);
  return [...named, { key: OTHER, count: rest, slot: null }];
}

/// Category → slot, with every category the legend folded mapping to null.
export function slotOf(legend: LegendEntry[]): (key: string) => number | null {
  const m = new Map(legend.map((e) => [e.key, e.slot]));
  return (key) => m.get(key) ?? null;
}

/// Data space → screen: `screen = data · scale + offset`, y flipped so
/// the map reads the way it was laid out.
export type View = { scale: number; tx: number; ty: number };

export function fitView(points: { x: number; y: number }[], w: number, h: number, pad = 24): View {
  if (points.length === 0 || w <= 0 || h <= 0) return { scale: 1, tx: w / 2, ty: h / 2 };
  let [x0, x1, y0, y1] = [Infinity, -Infinity, Infinity, -Infinity];
  for (const p of points) {
    x0 = Math.min(x0, p.x);
    x1 = Math.max(x1, p.x);
    y0 = Math.min(y0, p.y);
    y1 = Math.max(y1, p.y);
  }
  const span = Math.max(x1 - x0, y1 - y0, 1e-6);
  const scale = Math.min(
    (w - 2 * pad) / Math.max(x1 - x0, span * 1e-3),
    (h - 2 * pad) / Math.max(y1 - y0, span * 1e-3),
  );
  return { scale, tx: w / 2 - ((x0 + x1) / 2) * scale, ty: h / 2 + ((y0 + y1) / 2) * scale };
}

export function toScreen(v: View, x: number, y: number): [number, number] {
  return [x * v.scale + v.tx, -y * v.scale + v.ty];
}

export function toData(v: View, sx: number, sy: number): [number, number] {
  return [(sx - v.tx) / v.scale, -(sy - v.ty) / v.scale];
}

/// Zoom by `factor` keeping the data point under (sx, sy) where it is.
export function zoomAt(v: View, sx: number, sy: number, factor: number): View {
  const scale = v.scale * factor;
  return { scale, tx: sx - (sx - v.tx) * factor, ty: sy - (sy - v.ty) * factor };
}

/// A uniform grid over the points in data space, for "which point is
/// under the pointer" without scanning every point on every move.
export class PointGrid {
  private cells = new Map<string, number[]>();
  constructor(
    private xs: Float32Array,
    private ys: Float32Array,
    private cell: number,
  ) {
    for (let i = 0; i < xs.length; i++) {
      const k = this.key(Math.floor(xs[i] / cell), Math.floor(ys[i] / cell));
      const bucket = this.cells.get(k);
      if (bucket) bucket.push(i);
      else this.cells.set(k, [i]);
    }
  }

  static over(points: { x: number; y: number }[]): PointGrid {
    const xs = Float32Array.from(points, (p) => p.x);
    const ys = Float32Array.from(points, (p) => p.y);
    let [x0, x1, y0, y1] = [Infinity, -Infinity, Infinity, -Infinity];
    for (let i = 0; i < xs.length; i++) {
      x0 = Math.min(x0, xs[i]);
      x1 = Math.max(x1, xs[i]);
      y0 = Math.min(y0, ys[i]);
      y1 = Math.max(y1, ys[i]);
    }
    // About one point per cell on average, over the map's extent — but
    // never finer than a thousandth of its span, which a map lying on a
    // line (no area at all) would otherwise ask for.
    const n = Math.max(xs.length, 1);
    const span = Math.max(x1 - x0, y1 - y0, 0);
    const cell = Math.max(Math.sqrt(((x1 - x0) * (y1 - y0)) / n), span / 1000, 1e-6);
    return new PointGrid(xs, ys, cell);
  }

  private key(cx: number, cy: number): string {
    return `${cx},${cy}`;
  }

  /// The nearest point within `r` of (x, y) that `keep` admits, or -1.
  nearest(x: number, y: number, r: number, keep: (i: number) => boolean = () => true): number {
    const reach = Math.ceil(r / this.cell);
    let best = -1;
    let bestD = r * r;
    // Zoomed far out, the reach covers more cells than hold points:
    // scanning the points is then the cheaper walk.
    if ((2 * reach + 1) ** 2 > this.cells.size) {
      for (let i = 0; i < this.xs.length; i++) {
        const d = (this.xs[i] - x) ** 2 + (this.ys[i] - y) ** 2;
        if (d <= bestD && keep(i)) {
          bestD = d;
          best = i;
        }
      }
      return best;
    }
    const cx = Math.floor(x / this.cell);
    const cy = Math.floor(y / this.cell);
    for (let dx = -reach; dx <= reach; dx++) {
      for (let dy = -reach; dy <= reach; dy++) {
        for (const i of this.cells.get(this.key(cx + dx, cy + dy)) ?? []) {
          const d = (this.xs[i] - x) ** 2 + (this.ys[i] - y) ** 2;
          if (d <= bestD && keep(i)) {
            bestD = d;
            best = i;
          }
        }
      }
    }
    return best;
  }
}

/// The opening of a document's body as plain text, for a hover card:
/// markup and markdown syntax dropped, whitespace folded, cut at a word.
export function previewText(body: string, max = 360): string {
  const text = body
    .replace(/<[^>]*>/g, " ")
    .replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}(#{1,6}|>|[-*+]|\d+\.)\s+/gm, "")
    // A table's rule rows go, and its cells run on separated by a dot.
    .replace(/^[\s|:-]*-{3,}[\s|:-]*$/gm, "")
    .replace(/(\s*\|\s*)+/g, " · ")
    .replace(/[*_`~]+/g, "")
    .replace(/\s+/g, " ")
    .replace(/^(\s*·\s*)+|(\s*·\s*)+$/g, "")
    .trim();
  if (text.length <= max) return text;
  const cut = text.slice(0, max);
  const space = cut.lastIndexOf(" ");
  return `${space > max * 0.6 ? cut.slice(0, space) : cut}…`;
}

/// What the card keeps across a re-run: the filter, the colouring and
/// the point last opened. Opaque to the host; written only on a change
/// a person made, so a pristine card keeps clean state.
export type MapState = { q: string; by: ColorBy; sel: string | null };

export function decodeState(s: string): MapState {
  const p = new URLSearchParams(s);
  return { q: p.get("q") ?? "", by: parseColorBy(p.get("by")), sel: p.get("sel") };
}

export function encodeState(st: MapState): string {
  const p = new URLSearchParams();
  if (st.q) p.set("q", st.q);
  if (st.by !== "provider") p.set("by", st.by);
  if (st.sel) p.set("sel", st.sel);
  return p.toString();
}

/// The TOML stanza that declares the step, for a config that lacks it:
/// it reads every source's embeddings, so it runs after any of them.
export function stepStanza(embedIds: string[]): string {
  const inputs = embedIds.map((id) => JSON.stringify(id)).join(", ");
  return `
# qmd's document embeddings laid out on a plane, for the map card. Each
# run starts from the last map; resetting the step lays one out afresh.
[[steps]]
group = "unified_index"
function = "embedding_map"
inputs = [${inputs}]
`;
}

/// The steps the map reads: each source's `embed`.
export function embedStepIds(steps: { id: string }[]): string[] {
  return steps.map((s) => s.id).filter((id) => /^[^/]+\/embed$/.test(id));
}
