import { describe, expect, it } from "vitest";
import {
  asking,
  firstWindow,
  MAX_LIMIT,
  newestFirst,
  nextFetch,
  PAGE,
  refreshLimit,
  withNewer,
  withoutPage,
  withPage,
  type Page,
} from "./pagedWindow";

const rows = (from: number, n: number) => Array.from({ length: n }, (_, i) => from + i);
/// A page of a search: read on by offset, `at` a commit.
const page = (from: number, n: number, total: number, at = "c1"): Page<number, number> => ({
  rows: rows(from, n),
  total,
  next: from + n < total ? from + n : null,
  at,
});

describe("nextFetch", () => {
  const w = firstWindow(page(0, PAGE, 1000));

  /// A viewport inside the loaded rows asks for nothing.
  it("asks for nothing while the wanted rows are loaded", () => {
    expect(nextFetch(w, PAGE - 1)).toBeNull();
  });

  it("asks for the next page once the wanted rows run past the end", () => {
    expect(nextFetch(w, PAGE)).toEqual({ from: PAGE, limit: PAGE });
  });

  /// A jump far down the list loads through it in one request rather
  /// than a page at a time.
  it("asks for everything up to a row far past the end at once", () => {
    expect(nextFetch(w, 700)).toEqual({ from: PAGE, limit: 501 });
  });

  /// The log reads longer pages than the search.
  it("asks for at least the page size it is given", () => {
    expect(nextFetch(w, PAGE, 500)).toEqual({ from: PAGE, limit: 500 });
  });

  /// Grouping and header filters need every row.
  it("asks for the rest of the list, up to what one request carries", () => {
    expect(nextFetch(w, Infinity)).toEqual({ from: PAGE, limit: MAX_LIMIT });
  });

  it("asks for nothing once every row is loaded", () => {
    expect(nextFetch(firstWindow(page(0, 50, 50)), Infinity)).toBeNull();
  });

  /// A scroll fires many viewport events; only one page goes out at a time.
  it("asks for nothing while a page is on its way", () => {
    expect(nextFetch(asking(w, { from: PAGE, limit: PAGE }), 900)).toBeNull();
  });
});

describe("withPage", () => {
  const w = asking(firstWindow(page(0, PAGE, 1000)), { from: PAGE, limit: PAGE });

  it("appends the page and takes its cursor", () => {
    const next = withPage(w, PAGE, page(PAGE, PAGE, 1000));
    expect(next).not.toBe("moved");
    const got = next as Exclude<typeof next, "moved">;
    expect(got.rows).toEqual(rows(0, 2 * PAGE));
    expect(got.next).toBe(2 * PAGE);
    expect(got.pending).toBeNull();
  });

  /// The index sealed between two pages: appending would splice rows
  /// from two different lists.
  it("says the list moved when the page was read at something else", () => {
    expect(withPage(w, PAGE, page(PAGE, PAGE, 1001, "c2"))).toBe("moved");
  });

  /// A page for a window since replaced (a new query, a new sort) is
  /// dropped, not appended to rows it does not belong after.
  it("drops a page nobody is waiting for", () => {
    const fresh = firstWindow(page(0, PAGE, 1000));
    expect(withPage(fresh, PAGE, page(PAGE, PAGE, 1000))).toBe(fresh);
  });
});

describe("withoutPage", () => {
  /// A failed page must not wedge the window: the next scroll asks again.
  it("frees the window to ask again", () => {
    const w = asking(firstWindow(page(0, PAGE, 1000)), { from: PAGE, limit: PAGE });
    expect(nextFetch(withoutPage(w, PAGE), PAGE)).toEqual({ from: PAGE, limit: PAGE });
  });

  it("leaves a window waiting on another page alone", () => {
    const w = asking(firstWindow(page(0, PAGE, 1000)), { from: PAGE, limit: PAGE });
    expect(withoutPage(w, 0)).toBe(w);
  });
});

describe("a log read back from its newest line", () => {
  type Line = { seq: number };
  const lines = (from: number, n: number): Line[] => rows(from, n).map((seq) => ({ seq }));
  const bySeq = (l: Line) => l.seq;

  /// The log answers oldest first; the window holds newest first, and the
  /// next page is the one before the oldest line held.
  it("holds a full page newest first and pages back from its oldest line", () => {
    const w = firstWindow(newestFirst(lines(96, 5), 5, bySeq));
    expect(w.rows.map(bySeq)).toEqual([100, 99, 98, 97, 96]);
    expect(nextFetch(w, 5, 5)).toEqual({ from: 96, limit: 5 });
  });

  /// A short page reached the first line of the log.
  it("knows a short page is the last", () => {
    expect(newestFirst(lines(1, 3), 5, bySeq).next).toBeNull();
    expect(newestFirst([], 5, bySeq).next).toBeNull();
  });

  /// The tail's lines go on the near end, ahead of what is held.
  it("puts lines written since at the near end", () => {
    const w = firstWindow(newestFirst(lines(96, 5), 5, bySeq));
    expect(withNewer(w, lines(101, 2).reverse()).rows.map(bySeq)).toEqual([
      102, 101, 100, 99, 98, 97, 96,
    ]);
    expect(withNewer(w, [])).toBe(w);
  });
});

describe("refreshLimit", () => {
  /// Someone scrolled a thousand rows down keeps those rows when the
  /// index moves, rather than being cut back to the first page.
  it("reads again as many rows as are loaded", () => {
    expect(refreshLimit(firstWindow(page(0, 1000, 5000)))).toBe(1000);
  });

  it("reads at least a page", () => {
    expect(refreshLimit(firstWindow(page(0, 3, 3)))).toBe(PAGE);
  });
});
