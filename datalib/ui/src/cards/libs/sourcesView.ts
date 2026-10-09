// `sourcesView()` in card source: the Manage screen as a card — the
// tree of what config.toml declares, with its actions and panels. See
// cards/SourcesCard.ce.vue; its logic is cards/sourcesCardModel.ts.
import SourcesCard from "../SourcesCard.ce.vue";
import sourcesCardCss from "../sourcesCard.css?inline";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function sourcesView(opts?: { add?: boolean }): CardRender {
  return vueCard(
    SourcesCard,
    { add: opts?.add === true },
    { styleSources: [slickCss, tableGridCss, sourcesCardCss] },
  );
}
