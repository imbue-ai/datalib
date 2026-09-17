// `tableView({ url })` in card source: the typed table viewer over any
// endpoint that declares its columns. See cards/TableCard.ce.vue.
import TableCard from "../TableCard.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function tableView(opts: { url: string }): CardRender {
  return vueCard(TableCard, { url: opts.url }, { styleSources: [slickCss, tableGridCss] });
}
