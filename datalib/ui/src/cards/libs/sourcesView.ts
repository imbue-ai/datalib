// `sourcesView()` in card source: the Manage screen as a card — the
// tree of what config.toml declares, with its actions and panels. See
// cards/SourcesCard.ce.vue. The panels it opens are teleported out of
// the shadow root, so the card's styles are also imported into the
// head here.
import SourcesCard from "../SourcesCard.ce.vue";
import sourcesCardCss from "../sourcesCard.css?inline";
import tableGridCss from "../tableGrid.css?inline";
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import "../sourcesCard.css";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function sourcesView(): CardRender {
  return vueCard(SourcesCard, {}, { styleSources: [slickCss, tableGridCss, sourcesCardCss] });
}
