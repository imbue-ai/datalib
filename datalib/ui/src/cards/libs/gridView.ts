// The two card sources GridCard.ce.vue draws.
//
// `searchView()` is the Search card: one query over the unified index,
// shown as a list with a preview or as a table. It opens on the view
// picked last unless `view` names one. A search given a `name` keeps
// it; one without names itself after the live query; no `q` opens on
// `DEFAULT_QUERY`.
//
// `gridView({ url })` is the general-purpose grid: the table alone,
// over any endpoint that pages, sorts and groups the way the search
// does (the problems). It has no views or source chips, and
// its `url` is required.
import GridCard from "../GridCard.ce.vue";
import SearchList from "../SearchList.ce.vue";
import { DEFAULT_QUERY } from "../searchDefaults";
import type { SearchViewId } from "../searchViewPref";
import tableGridCss from "../tableGrid.css?inline";
import chipCss from "../chip.css?inline";
// The grid's theme has to be in the same root as the grid; head
// styles stop at the shadow boundary.
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export type SearchOpts = {
  q?: string;
  columns?: string[];
  name?: string;
  view?: SearchViewId;
};

export type GridOpts = {
  url: string;
  q?: string;
  columns?: string[];
  name?: string;
  placeholder?: string;
};

const STYLES = { styleSources: [SearchList, slickCss, tableGridCss, chipCss] };

export function searchView(opts?: SearchOpts): CardRender {
  return vueCard(
    GridCard,
    {
      q: opts?.q || DEFAULT_QUERY,
      columns: opts?.columns,
      name: opts?.name,
      view: opts?.view,
    },
    STYLES,
  );
}

export function gridView(opts?: GridOpts): CardRender {
  if (!opts?.url) {
    throw new Error(
      'gridView needs the url of the table to show, as in gridView({ url: "/applet/unified_index/problems" }). The search is searchView().',
    );
  }
  return vueCard(
    GridCard,
    {
      url: opts.url,
      q: opts.q ?? "",
      columns: opts.columns,
      name: opts.name,
      placeholder: opts.placeholder,
    },
    STYLES,
  );
}
