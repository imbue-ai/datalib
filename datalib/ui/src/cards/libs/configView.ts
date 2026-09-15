// `configView()` in card source: config.toml, edited directly. See
// cards/ConfigCard.ce.vue.
import ConfigCard from "../ConfigCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function configView(): CardRender {
  return vueCard(ConfigCard, {});
}
