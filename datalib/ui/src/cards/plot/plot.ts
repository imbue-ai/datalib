// What every chart in the UI shares, drawn by uPlot: the categorical
// palette, colours read off the theme's CSS tokens, the axis look, and
// the rules of the legend. The Vue lifecycle around a plot — building,
// resizing, zoom, a theme change, a plot inside a document's frame — is
// usePlot.ts; the legend itself is PlotLegend.vue; the stylesheet every
// root that holds a plot needs is plotCss.ts. docs/dev/cards.md §"Charts".
import type uPlot from "uplot";

/// How many categorical colours the theme defines (`--viz-series-N`).
export const SERIES_SLOTS = 6;

/// The `i`th categorical colour, as CSS: a swatch takes it as it is, and
/// a plot resolves it with [`resolveColor`].
export function seriesColor(i: number): string {
  return `var(--viz-series-${(i % SERIES_SLOTS) + 1})`;
}

/// The token a `var(--token)` or a bare `--token` names; null for a
/// colour given as itself.
export function tokenOf(color: string): string | null {
  if (color.startsWith("--")) return color;
  return /^var\(\s*(--[\w-]+)\s*\)$/.exec(color)?.[1] ?? null;
}

/// A colour as a canvas takes it: a token read off `el` now, anything
/// else as given. A canvas cannot take `var()`, so a plot reads every
/// colour when it is built and is rebuilt when the colour scheme
/// changes. The window is `el`'s own, since a plot in a document lives
/// in the document's frame.
export function resolveColor(el: Element, color: string): string {
  const token = tokenOf(color);
  if (!token) return color;
  const view = el.ownerDocument.defaultView ?? window;
  return view.getComputedStyle(el).getPropertyValue(token).trim();
}

/// `color` at `alpha` opacity, for a fill under or between lines. Takes
/// what [`resolveColor`] gives: `#rgb`, `#rrggbb` or `rgb(…)`.
export function withAlpha(color: string, alpha: number): string {
  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(color)?.[1];
  if (hex) {
    const full = hex.length === 3 ? [...hex].map((c) => c + c).join("") : hex;
    const [r, g, b] = [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16));
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }
  const rgb = /^rgba?\(([^)]+)\)$/.exec(color)?.[1];
  if (rgb) {
    const [r, g, b] = rgb.split(/[\s,/]+/).filter(Boolean);
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }
  throw new Error(`withAlpha: not a colour a canvas fill can fade: ${color}`);
}

/// The theme as one plot was built in.
export type Theme = {
  color: (c: string) => string;
  ink: string;
  muted: string;
  grid: string;
  bg: string;
  font: string;
  /// For a chart a hundred pixels tall, such as the sync dashboard's.
  fontSmall: string;
};

export function themeOf(el: Element): Theme {
  const color = (c: string) => resolveColor(el, c);
  return {
    color,
    ink: color("--datalib-fg"),
    muted: color("--datalib-muted"),
    grid: color("--datalib-border-soft"),
    bg: color("--datalib-bg"),
    font: "11px system-ui, sans-serif",
    fontSmall: "10px system-ui, sans-serif",
  };
}

/// An axis in the app's look: muted labels, a soft grid on value axes,
/// none on the time axis. `over` sets the rest (side, scale, splits,
/// values, size).
export function axis(theme: Theme, kind: "time" | "value", over: uPlot.Axis = {}): uPlot.Axis {
  return {
    stroke: theme.muted,
    font: theme.font,
    grid: kind === "value" ? { stroke: theme.grid, width: 1 } : { show: false },
    ticks: kind === "time" ? { stroke: theme.grid, width: 1, size: 4 } : { show: false },
    ...over,
  };
}

/// Which series are shown after a click on the `i`th legend entry: it
/// alone turns on or off.
export function toggled(shown: boolean[], i: number): boolean[] {
  return shown.map((s, j) => (j === i ? !s : s));
}

/// Which series are shown after a double-click on the `i`th entry: that
/// one alone, or every one again when it already was alone.
export function isolated(shown: boolean[], i: number): boolean[] {
  const alone = shown.every((s, j) => s === (j === i));
  return shown.map((_, j) => alone || j === i);
}
