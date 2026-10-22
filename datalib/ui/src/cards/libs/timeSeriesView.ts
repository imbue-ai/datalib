// `timeSeriesView()` in card source: every time series in the time-series
// index, picked from a list and drawn on one chart. See
// cards/TimeSeriesCard.ce.vue.
import TimeSeriesCard from "../TimeSeriesCard.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import EntityChip from "../EntityChip.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function timeSeriesView(): CardRender {
  return vueCard(TimeSeriesCard, {}, { styleSources: [EntityChip, ChipMenu, chipCss] });
}
