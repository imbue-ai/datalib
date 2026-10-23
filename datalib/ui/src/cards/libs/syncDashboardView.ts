// `syncDashboardView({ group, step })` in card source: the older name of a
// source's card (`sourceView`), opening on its Sync tab. Kept so a saved
// layout or a link written before still opens.
import { sourceView } from "./sourceView";
import type { CardRender } from "../types";
import type { SyncDashboardOpts } from "../cardSources";

export { syncDashboardSource, type SyncDashboardOpts } from "../cardSources";

export function syncDashboardView(opts: SyncDashboardOpts): CardRender {
  return sourceView({ ...opts, tab: "sync" });
}
