// `historyView({ trees, title, source, compare })` in card source: the
// commit history of the doltlite stores under some trees, and the place
// a source's two versions are compared. See cards/HistoryCard.ce.vue.
import HistoryCard from "../HistoryCard.ce.vue";
import historyCardCss from "../historyCard.css?inline";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export type HistoryViewOpts = {
  /// Group or step ids; every store under each is listed.
  trees: string[];
  /// What the history is of, for the card's title.
  title: string;
  /// The source whose download's commits can be compared, when the
  /// history is a comparable source's.
  source?: string | null;
  /// Open with a comparison of the newest two commits set up.
  compare?: boolean;
};

export function historyView(opts: HistoryViewOpts): CardRender {
  return vueCard(HistoryCard, { opts }, { styleSources: [slickCss, tableGridCss, historyCardCss] });
}

/// The source of a history card, for whoever opens one.
export function historySource(opts: HistoryViewOpts): string {
  return `historyView(${JSON.stringify(opts)})`;
}
