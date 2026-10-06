// `searchView({ q })` in card source: the Search card — results as a
// list, filtered by source, the picked one read in place (see
// cards/SearchCard.ce.vue). The same search as `gridView`, which shows
// it as a table; each opens the other on the same query. The preview
// is the document card itself, so its stylesheets come along.
import SearchCard from "../SearchCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function searchView(opts?: { q?: string }): CardRender {
  return vueCard(SearchCard, { q: opts?.q ?? "" });
}
