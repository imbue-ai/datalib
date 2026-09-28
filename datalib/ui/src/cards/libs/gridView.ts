// `gridView()` in card source returns a CardRender for the search
// grid card (see cards/GridCard.ce.vue). A grid given a `name` keeps
// it; one without names itself after the live query. `url` points it at
// another table that pages the way the search does (the problems).
import GridCard from "../GridCard.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
// The grid's theme has to be in the same root as the grid; head
// styles stop at the shadow boundary.
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export function gridView(opts?: {
  q?: string;
  columns?: string[];
  name?: string;
  url?: string;
  placeholder?: string;
}): CardRender {
  return vueCard(
    GridCard,
    {
      q: opts?.q ?? "",
      columns: opts?.columns,
      name: opts?.name,
      url: opts?.url,
      placeholder: opts?.placeholder,
    },
    { styleSources: [slickCss, tableGridCss] },
  );
}
