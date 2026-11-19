// `libraryTreeView()` in card source: the library as a tree, the node
// opened in it shown beside. See cards/LibraryCard.ce.vue.
import LibraryCard from "../LibraryCard.ce.vue";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function libraryTreeView(): CardRender {
  return vueCard(LibraryCard);
}
