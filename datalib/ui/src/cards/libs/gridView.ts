// `gridView()` in card source returns a CardRender for the search
// grid card (see cards/GridCard.ce.vue). The card titles itself
// there, via ctx.setTitle, tracking the live query.
import GridCard from "../GridCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function gridView(opts?: { q?: string }): CardRender {
  return vueCard(GridCard, { q: opts?.q ?? "" });
}
