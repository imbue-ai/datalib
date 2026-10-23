// `sourceView({ group, step, tab })` in card source: one source as one
// card — its row's status and actions at the top, then its documents
// (the browse grid, with the columns that suit its type) and its sync
// (each step's section, charts over the run, the group's log). A step
// opens the Sync tab at that step's section (cards/SourceCard.ce.vue).
import SourceCard from "../SourceCard.ce.vue";
import GridCard from "../GridCard.ce.vue";
import SearchList from "../SearchList.ce.vue";
import TimeChart from "../TimeChart.ce.vue";
import RunLogPanel from "@/components/RunLogPanel.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import EntityChip from "../EntityChip.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
import chipCss from "../chip.css?inline";
import { PLOT_CSS } from "../plot/plotCss";
// The grids' theme has to be in the same root as the grids.
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";
import type { SourceViewOpts } from "../cardSources";

export { sourceSource, type SourceViewOpts } from "../cardSources";

export function sourceView(opts: SourceViewOpts): CardRender {
  return vueCard(
    SourceCard,
    { opts },
    {
      styleSources: [
        slickCss,
        tableGridCss,
        chipCss,
        PLOT_CSS,
        TimeChart,
        RunLogPanel,
        EntityChip,
        ChipMenu,
        GridCard,
        SearchList,
      ],
    },
  );
}
