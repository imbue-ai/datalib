import { describe, expect, it } from "vitest";
import {
  asking,
  firstWindow,
  MAX_LIMIT,
  nextFetch,
  PAGE,
  refreshLimit,
  withoutPage,
  withPage,
  type Page,
} from "./pagedWindow";

const rows = (from: number, n: number) => Array.from({ length: n }, (_, i) => from + i);
const page = (from: number, n: number, total: number, at = "c1"): Page<number> => ({
  rows: rows(from, n),
  total,
  next_offset: from + n < total ? from + n : null,
  at,
});

describe("nextFetch", () => {
  const w = firstWindow(page(0, PAGE, 1000));

  /// A viewport inside the loaded rows asks for nothing.
  it("asks for nothing while the wanted rows are loaded", () => {
    expect(nextFetch(w, PAGE - 1)).toBeNull();
  });

  it("asks for the next page once the wanted rows run past the end", () => {
    expect(nextFetch(w, PAGE)).toEqual({ offset: PAGE, limit: PAGE });
  });

  /// A jump far down the list, a restored selection say, loads through it
  /// in one request rather than a page at a time.
  it("asks for everything up to a row far past the end at once", () => {
    expect(nextFetch(w, 700)).toEqual({ offset: PAGE, limit: 501 });
  });

  /// Grouping and header filters need every row.
  it("asks for the rest of the search, up to what one request carries", () => {
    expect(nextFetch(w, Infinity)).toEqual({ offset: PAGE, limit: MAX_LIMIT });
  });

  it("asks for nothing once every row is loaded", () => {
    expect(nextFetch(firstWindow(page(0, 50, 50)), Infinity)).toBeNull();
  });

  /// A scroll fires many viewport events; only one page goes out at a time.
  it("asks for nothing while a page is on its way", () => {
    expect(nextFetch(asking(w, { offset: PAGE, limit: PAGE }), 900)).toBeNull();
  });
});

describe("withPage", () => {
  const w = asking(firstWindow(page(0, PAGE, 1000)), { offset: PAGE, limit: PAGE });

  it("appends the page and takes its cursor", () => {
    const next = withPage(w, PAGE, page(PAGE, PAGE, 1000));
    expect(next).not.toBe("moved");
    const got = next as Exclude<typeof next, "moved">;
    expect(got.rows).toEqual(rows(0, 2 * PAGE));
    expect(got.nextOffset).toBe(2 * PAGE);
    expect(got.pending).toBeNull();
  });

  /// The index sealed between two pages: appending would splice rows
  /// from two different lists.
  it("says the index moved when the page was read at another commit", () => {
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
    const w = asking(firstWindow(page(0, PAGE, 1000)), { offset: PAGE, limit: PAGE });
    expect(nextFetch(withoutPage(w, PAGE), PAGE)).toEqual({ offset: PAGE, limit: PAGE });
  });

  it("leaves a window waiting on another page alone", () => {
    const w = asking(firstWindow(page(0, PAGE, 1000)), { offset: PAGE, limit: PAGE });
    expect(withoutPage(w, 0)).toBe(w);
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
