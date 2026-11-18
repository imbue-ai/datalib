// `tableView({ url })` in card source: the typed table viewer over one
// of the app's tables that declare their columns (`TYPED_TABLES`). See
// cards/TableCard.ce.vue.
import TableCard from "../TableCard.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { listedTable, TYPED_TABLES } from "../tableUrls";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function tableView(opts?: { url: string; title?: string }): CardRender {
  return vueCard(
    TableCard,
    { url: listedTable("tableView", opts?.url, TYPED_TABLES), title: opts?.title },
    { styleSources: [slickCss, tableGridCss] },
  );
}
