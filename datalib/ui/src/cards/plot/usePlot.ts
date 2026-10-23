// One uPlot plot's life inside a Vue component, the same for every
// chart: built when the component mounts and whenever what it is built
// for changes, given new data otherwise, sized to its box, rebuilt in
// the new colours when the colour scheme changes, and destroyed with the
// component. With `zoom`, dragging along x zooms in and a double-click
// goes back; plots sharing a sync key zoom together. A plot in a
// document's frame works there too (`inFrame`).
import { onBeforeUnmount, onMounted, shallowRef, watch, type Ref } from "vue";
import uPlot from "uplot";
import { themeOf, type Theme } from "./plot";

export type Range = [number, number];

export type PlotSpec = {
  /// The plot's options in `theme`. usePlot adds the size, the sync key,
  /// and the cursor and x scale that zoom needs.
  options: (theme: Theme) => Omit<uPlot.Options, "width" | "height">;
  data: () => uPlot.AlignedData;
  /// What a plot is built for: when this changes the plot is built
  /// again; any other change is new data on the old plot.
  shape: () => unknown;
  /// A fixed height in CSS pixels; without one the plot fills its box.
  height?: number;
  minWidth?: number;
  /// Charts with one key share a crosshair, and a zoom.
  syncKey?: string;
  /// Zoom along x: `base` is the range shown when not zoomed, in the x
  /// scale's units; `changed` hears every zoom, and null when it is reset.
  zoom?: { base: () => Range; changed?: (range: Range | null) => void };
  /// Which of the plot's series (after x) are drawn; all when absent.
  shown?: () => boolean[];
  resized?: (width: number, height: number) => void;
};

/// What resets each live plot's zoom, so one double-click resets every
/// plot it is synced with.
const resetters = new WeakMap<uPlot, () => void>();

export function usePlot(box: Ref<HTMLElement | null>, spec: PlotSpec) {
  const plot = shallowRef<uPlot | null>(null);
  /// The x range zoomed to, or null for `zoom.base()`. Kept across a
  /// rebuild; the owner resets it when what is drawn changes.
  let zoomed: Range | null = null;
  const minWidth = spec.minWidth ?? 120;

  function size(el: HTMLElement) {
    return {
      width: Math.max(minWidth, Math.floor(el.clientWidth)),
      height: spec.height ?? Math.max(40, Math.floor(el.clientHeight)),
    };
  }

  function zoomTo(u: uPlot, range: Range | null) {
    zoomed = range;
    const [min, max] = range ?? spec.zoom!.base();
    u.setScale("x", { min, max });
    spec.zoom!.changed?.(range);
  }

  function withZoom(opts: uPlot.Options): uPlot.Options {
    const zoom = spec.zoom;
    if (!zoom) return opts;
    return {
      ...opts,
      scales: { ...opts.scales, x: { ...opts.scales?.x, range: () => zoomed ?? zoom.base() } },
      cursor: {
        ...opts.cursor,
        drag: { x: true, y: false, setScale: false },
        // uPlot's own double-click fits the data, which after a zoom is
        // only the zoomed range's; ours goes back to `base`.
        bind: { ...opts.cursor?.bind, dblclick: () => null },
      },
      hooks: {
        ...opts.hooks,
        setSelect: [
          ...(opts.hooks?.setSelect ?? []),
          (u) => {
            const { left, width } = u.select;
            if (width < 2) return;
            u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
            zoomTo(u, [u.posToVal(left, "x"), u.posToVal(left + width, "x")]);
          },
        ],
      },
    };
  }

  /// Undoes what `inFrame` attached for the plot now drawn.
  let detach = new AbortController();

  function destroy() {
    detach.abort();
    plot.value?.destroy();
    plot.value = null;
  }

  function build() {
    destroy();
    const el = box.value;
    if (!el) return;
    const base = spec.options(themeOf(el));
    const opts = withZoom({
      ...base,
      ...size(el),
      // A line turned off in one synced chart is its own chart's
      // business: the next chart's line of that index is another thing.
      cursor: {
        ...base.cursor,
        ...(spec.syncKey ? { sync: { key: spec.syncKey, setSeries: false } } : {}),
      },
    });
    const u = new uPlot(opts, spec.data(), el);
    if (spec.zoom) {
      resetters.set(u, () => zoomTo(u, null));
      u.over.addEventListener("dblclick", () => {
        const peers = spec.syncKey ? uPlot.sync(spec.syncKey).plots : [u];
        for (const p of peers) resetters.get(p)?.();
      });
    }
    detach = new AbortController();
    inFrame(u, el, detach.signal);
    plot.value = u;
    applyShown();
  }

  /// Draws the series `shown` asks for, and says which are drawn on the
  /// box as `data-shown` ("1" or "0" a series), which is what a test of
  /// the legend reads off a canvas it cannot see into.
  function applyShown() {
    const u = plot.value;
    if (!u) return;
    const want = spec.shown?.() ?? [];
    u.batch(() => {
      want.forEach((show, i) => {
        if (u.series[i + 1] && u.series[i + 1]!.show !== show) u.setSeries(i + 1, { show });
      });
    });
    box.value!.dataset.shown = u.series
      .slice(1)
      .map((s) => (s.show ? "1" : "0"))
      .join("");
  }

  // A plot in a frame: uPlot listens on the window and the document it
  // was loaded in, which are the app's, while the plot's events happen in
  // the frame's. So a drag's mouseup, and a scroll that moves the plot,
  // are passed on to where uPlot listens.
  function inFrame(u: uPlot, el: HTMLElement, signal: AbortSignal) {
    const doc = el.ownerDocument;
    if (doc === document) return;
    const forward = (e: MouseEvent) => {
      const { clientX, clientY, button, buttons } = e;
      document.dispatchEvent(
        new MouseEvent("mouseup", { clientX, clientY, button, buttons, bubbles: true }),
      );
    };
    u.over.addEventListener(
      "mousedown",
      () => doc.addEventListener("mouseup", forward, { once: true, signal }),
      { signal },
    );
    doc.defaultView?.addEventListener("scroll", () => u.syncRect(true), {
      capture: true,
      passive: true,
      signal,
    });
  }

  /// Back to `zoom.base()` without telling `zoom.changed`: for the owner,
  /// when what it draws has changed under the zoom.
  function resetZoom() {
    zoomed = null;
    const u = plot.value;
    if (!u || !spec.zoom) return;
    const [min, max] = spec.zoom.base();
    u.setScale("x", { min, max });
  }

  watch(spec.shape, build);
  watch(spec.data, (d) => plot.value?.setData(d));
  if (spec.shown) watch(spec.shown, applyShown);

  let observer: ResizeObserver | null = null;
  let scheme: MediaQueryList | null = null;
  onMounted(() => {
    const el = box.value;
    if (!el) return;
    build();
    const view = el.ownerDocument.defaultView ?? window;
    observer = new view.ResizeObserver(() => {
      const s = size(el);
      plot.value?.setSize(s);
      spec.resized?.(s.width, s.height);
    });
    observer.observe(el);
    scheme = view.matchMedia("(prefers-color-scheme: dark)");
    scheme.addEventListener("change", build);
  });
  onBeforeUnmount(() => {
    observer?.disconnect();
    scheme?.removeEventListener("change", build);
    destroy();
  });

  return { plot, resetZoom };
}
