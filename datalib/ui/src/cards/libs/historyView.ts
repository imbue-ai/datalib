// `historyView({ trees, title, source, compare })` in card source: the
// commit history of the doltlite stores under some trees, and the place
// a source's two versions are compared. See cards/HistoryCard.ce.vue.
import HistoryCard from "../HistoryCard.ce.vue";
import historyCardCss from "../historyCard.css?inline";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";
import type { HistoryViewOpts } from "../cardSources";

export { historySource, type HistoryViewOpts } from "../cardSources";

export function historyView(opts: HistoryViewOpts): CardRender {
  return vueCard(HistoryCard, { opts }, { styleSources: [slickCss, tableGridCss, historyCardCss] });
}
