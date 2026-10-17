// A render asks for a chart by writing `<div data-time-series="<id> <id>">`
// (docs/dev/time_series.md §"Charts in documents"). The document view
// calls `mountTimeSeries` after each paint; the chart is the app's own
// code reaching into the frame, which runs no script of its own.
import { createApp, type App } from "vue";
import TimeSeriesEmbed from "./TimeSeriesEmbed.vue";

const mounted = new WeakMap<Element, App>();

/// Mount a chart into every `[data-time-series]` under `root` that holds
/// none yet. Returns what unmounts the ones this call mounted.
export function mountTimeSeries(root: ParentNode): () => void {
  const apps: App[] = [];
  for (const el of root.querySelectorAll<HTMLElement>("[data-time-series]")) {
    if (mounted.has(el)) continue;
    const ids = (el.dataset.timeSeries ?? "").split(/\s+/).filter(Boolean);
    if (!ids.length) continue;
    const app = createApp(TimeSeriesEmbed, { ids });
    app.mount(el);
    mounted.set(el, app);
    apps.push(app);
  }
  return () => {
    for (const app of apps) app.unmount();
  };
}
