// Which of the Search card's two views a new search opens in: the one
// the person picked last, in any search, kept in this browser. A card
// that was switched keeps its own view in its state; this is only where
// the next one starts.
export const SEARCH_VIEWS = ["list", "table"] as const;
export type SearchViewId = (typeof SEARCH_VIEWS)[number];

const KEY = "datalib-search-view";

export function lastSearchView(): SearchViewId {
  try {
    const kept = localStorage.getItem(KEY);
    return SEARCH_VIEWS.find((v) => v === kept) ?? "list";
  } catch {
    return "list";
  }
}

export function rememberSearchView(view: SearchViewId) {
  try {
    localStorage.setItem(KEY, view);
  } catch {
    // Blocked storage: the next search opens on the list.
  }
}
