// `umapView()` in card source: the embedding map (cards/UmapCard.ce.vue),
// every embedded document placed by what it says. `q` starts it
// filtered, as the grid's `q` does; `by` names the field it colours by.
import UmapCard from "../UmapCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function umapView(opts?: { q?: string; by?: string }): CardRender {
  return vueCard(UmapCard, { opts: { q: opts?.q, by: opts?.by } });
}
