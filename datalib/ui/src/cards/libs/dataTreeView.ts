// `dataTreeView()` in card source: the library as a tree of folders,
// sources and what each holds, opened a page at a time. See
// cards/DataTreeCard.ce.vue. `page` is how many children a page lists.
import DataTreeCard from "../DataTreeCard.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import EntityChip from "../EntityChip.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function dataTreeView(opts?: { page?: number }): CardRender {
  return vueCard(
    DataTreeCard,
    { opts: { page: opts?.page } },
    { styleSources: [EntityChip, ChipMenu, chipCss] },
  );
}
