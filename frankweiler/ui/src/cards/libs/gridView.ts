// `gridView()` in card source returns a CardRender for the search
// grid card (see cards/GridCard.ce.vue).
import GridCard from "../GridCard.ce.vue";
import { vueCard } from "../vueCard";
import { titled } from "../title";
import type { CardRender } from "../types";

export function gridView(opts?: { q?: string }): CardRender {
  const q = opts?.q ?? "";
  return titled(q ? `Search: ${q}` : "Search", vueCard(GridCard, { q }));
}
