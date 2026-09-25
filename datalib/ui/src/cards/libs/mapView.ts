// `mapView()` in card source: the embedding map (cards/MapCard.ce.vue),
// every embedded document placed by what it says. `q` starts it
// filtered, as the grid's `q` does; `by` names the field it colours by.
import MapCard from "../MapCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function mapView(opts?: { q?: string; by?: string }): CardRender {
  return vueCard(MapCard, { opts: { q: opts?.q, by: opts?.by } });
}
