// `logView({ run, step, launch })` in card source: the run log as a
// card (see cards/LogCard.ce.vue). Opened by the Manage card for a
// step, a run or the server; a line selected in it opens
// `logLineView(seq)` beside it.
import LogCard from "../LogCard.ce.vue";
import RunLogPanel from "@/components/RunLogPanel.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
import chipCss from "../chip.css?inline";
// The grid's theme has to be in the same root as the grid; head
// styles stop at the shadow boundary.
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";
import type { LogViewOpts } from "../cardSources";

export { logSource, type LogViewOpts } from "../cardSources";

export function logView(opts: LogViewOpts = {}): CardRender {
  return vueCard(
    LogCard,
    { opts },
    { styleSources: [slickCss, tableGridCss, chipCss, RunLogPanel] },
  );
}
