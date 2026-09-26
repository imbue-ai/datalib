// The rows of a paged search a grid holds: a prefix of the search's
// ordered rows, loaded a page at a time as the grid scrolls towards its
// end. Pure: the card feeds it what it wants loaded and what came back,
// and runs the fetches it asks for. See docs/dev/plans/paged_grids.md.

/// Rows per request while scrolling.
export const PAGE = 200;
/// How far past the last row on screen to have loaded, so a steady scroll
/// finds rows already there.
export const MARGIN = 100;
/// The most rows the search endpoint answers with at once.
export const MAX_LIMIT = 100_000;

/// One page of a search, as the endpoint answers it.
export type Page<Row> = {
  rows: Row[];
  total: number;
  next_offset: number | null;
  at: string | null;
};

export type PagedWindow<Row> = {
  rows: Row[];
  total: number;
  /// Where the next page starts, or null once every row is loaded.
  nextOffset: number | null;
  /// The index commit every loaded row was read at.
  at: string | null;
  /// The offset of the page on its way, if one is.
  pending: number | null;
};

export type Fetch = { offset: number; limit: number };

export function firstWindow<Row>(page: Page<Row>): PagedWindow<Row> {
  return {
    rows: page.rows,
    total: page.total,
    nextOffset: page.next_offset,
    at: page.at,
    pending: null,
  };
}

/// The page to ask for so that every row up to index `through` is loaded,
/// or null when they are, when there are no more, or when a page is
/// already on its way. `Infinity` asks for the rest of the search.
export function nextFetch<Row>(w: PagedWindow<Row>, through: number): Fetch | null {
  if (w.pending !== null || w.nextOffset === null || through < w.rows.length) return null;
  const wanted = through === Infinity ? MAX_LIMIT : through + 1 - w.nextOffset;
  return { offset: w.nextOffset, limit: Math.min(MAX_LIMIT, Math.max(PAGE, wanted)) };
}

/// Where the next fetch goes out.
export function asking<Row>(w: PagedWindow<Row>, f: Fetch): PagedWindow<Row> {
  return { ...w, pending: f.offset };
}

/// What a page that came back does to the window: extends it, or, when
/// the index moved since the window was read, is `"moved"`: the rows held
/// no longer line up with the search, and the caller reads them again.
/// A page nobody is waiting for any more changes nothing.
export function withPage<Row>(
  w: PagedWindow<Row>,
  offset: number,
  page: Page<Row>,
): PagedWindow<Row> | "moved" {
  if (w.pending !== offset) return w;
  if (page.at !== w.at) return "moved";
  return {
    rows: [...w.rows, ...page.rows],
    total: page.total,
    nextOffset: page.next_offset,
    at: page.at,
    pending: null,
  };
}

/// The window after a page failed to arrive: the same rows, free to ask
/// again.
export function withoutPage<Row>(w: PagedWindow<Row>, offset: number): PagedWindow<Row> {
  return w.pending === offset ? { ...w, pending: null } : w;
}

/// How many rows to read again when the index moves under a window, so a
/// person scrolled down the list keeps what they are looking at.
export function refreshLimit<Row>(w: PagedWindow<Row>): number {
  return Math.min(MAX_LIMIT, Math.max(PAGE, w.rows.length));
}
