// The rows of a long list a grid holds: a prefix of the list, loaded a
// page at a time as the grid nears the far end of what it holds. The
// search grid's list is a search, read on by offset; the run log's is the
// log newest first, read back by `seq`, and it also grows at its near end
// as lines are written. Pure: the card feeds it what it wants loaded and
// what came back, and runs the fetches it asks for. See
// docs/dev/plans/paged_grids.md.

/// Rows per request while scrolling.
export const PAGE = 200;
/// How far past the last row on screen to have loaded, so a steady scroll
/// finds rows already there.
export const MARGIN = 100;
/// The most rows the search endpoint answers with at once.
export const MAX_LIMIT = 100_000;

/// One page of the list, in its order, and where the page after it starts
/// (`C`, the cursor), or null when this one reaches the end.
export type Page<Row, C> = {
  rows: Row[];
  next: C | null;
  /// How long the whole list is, where the source says.
  total: number | null;
  /// What the page was read at, where the source says: a later page read
  /// at something else no longer lines up with this one.
  at: string | null;
};

export type PagedWindow<Row, C> = {
  rows: Row[];
  total: number | null;
  next: C | null;
  at: string | null;
  /// Where the page on its way starts, if one is.
  pending: C | null;
};

export type Fetch<C> = { from: C; limit: number };

export function firstWindow<Row, C>(page: Page<Row, C>): PagedWindow<Row, C> {
  return { rows: page.rows, total: page.total, next: page.next, at: page.at, pending: null };
}

/// A page read back from the list's newest end, oldest row first, as the
/// log answers: the window's order is newest first, and a full page means
/// there may be more before its oldest row.
export function newestFirst<Row, C>(
  oldestFirst: Row[],
  limit: number,
  cursorOf: (row: Row) => C,
): Page<Row, C> {
  const full = oldestFirst.length >= limit;
  return {
    rows: [...oldestFirst].reverse(),
    next: full ? cursorOf(oldestFirst[0]) : null,
    total: null,
    at: null,
  };
}

/// The page to ask for so that every row up to index `through` is held,
/// or null when they are, when there are no more, or when a page is
/// already on its way. `Infinity` asks for the rest of the list.
export function nextFetch<Row, C>(
  w: PagedWindow<Row, C>,
  through: number,
  pageSize = PAGE,
): Fetch<C> | null {
  if (w.pending !== null || w.next === null || through < w.rows.length) return null;
  const wanted = through === Infinity ? MAX_LIMIT : through + 1 - w.rows.length;
  return { from: w.next, limit: Math.min(MAX_LIMIT, Math.max(pageSize, wanted)) };
}

/// Where the next fetch goes out.
export function asking<Row, C>(w: PagedWindow<Row, C>, f: Fetch<C>): PagedWindow<Row, C> {
  return { ...w, pending: f.from };
}

/// What a page that came back does to the window: extends it, or, when
/// the list moved since the window was read, is `"moved"`: the rows held
/// no longer line up with it, and the caller reads them again. A page
/// nobody is waiting for any more changes nothing.
export function withPage<Row, C>(
  w: PagedWindow<Row, C>,
  from: C,
  page: Page<Row, C>,
): PagedWindow<Row, C> | "moved" {
  if (w.pending !== from) return w;
  if (page.at !== w.at) return "moved";
  return {
    rows: [...w.rows, ...page.rows],
    total: page.total,
    next: page.next,
    at: page.at,
    pending: null,
  };
}

/// The window after a page failed to arrive: the same rows, free to ask
/// again.
export function withoutPage<Row, C>(w: PagedWindow<Row, C>, from: C): PagedWindow<Row, C> {
  return w.pending === from ? { ...w, pending: null } : w;
}

/// Rows the list gained at its near end, newest first: the lines a log
/// has had written since the window was read.
export function withNewer<Row, C>(w: PagedWindow<Row, C>, newer: Row[]): PagedWindow<Row, C> {
  return newer.length === 0 ? w : { ...w, rows: [...newer, ...w.rows] };
}

/// How many rows to read again when the list moves under a window, so a
/// person scrolled down it keeps what they are looking at.
export function refreshLimit<Row, C>(w: PagedWindow<Row, C>): number {
  return Math.min(MAX_LIMIT, Math.max(PAGE, w.rows.length));
}
