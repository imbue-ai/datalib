// The search grid's order and filters all go to the server: a header
// click, and a shift-click to add a column, sort the whole search; a
// column dropped on the search bar becomes a term there.

import { test, expect, type APIRequestContext, type Page, type Route } from "@playwright/test";
import {
  EVERY_ROW,
  actOnRowByUuid,
  firstRowUuid,
  gridSettled,
  SEARCH_MENU,
  SEARCH_ROWS,
  searchHeader,
  searchMenuItem,
  type GridApi,
} from "./grid-helpers";

async function searchUuids(request: APIRequestContext, params: string): Promise<string[]> {
  const r = await request.get(`/applet/unified_document_index/search?${params}`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: { uuid: string }[] }).rows.map((row) => row.uuid);
}

async function openGrid(page: Page) {
  await page.goto(EVERY_ROW);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
}

const topRows = (page: Page, n: number) =>
  page.evaluate((n) => {
    const a = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
    return Array.from({ length: n }, (_, i) => a.uuidAt(i));
  }, n);

/// A shift-click adds a second column to the sort, and the server orders
/// the whole search by both, the first breaking ties by the second.
test("a shift-click sorts by a second column too", async ({ page, request }) => {
  const expected = await searchUuids(request, "q=&limit=10&sort=kind:asc,touched_at:desc");
  await openGrid(page);
  await searchHeader(page, "kind").click();
  await searchHeader(page, "touched_at").click({ modifiers: ["Shift"] });
  await expect.poll(() => topRows(page, 10)).toEqual(expected);
  await gridSettled(page);
  expect(await topRows(page, 10)).toEqual(expected);
});

/// A column header dropped on the search bar keeps the rows with a value
/// in it, as a term a person can read and edit: the Author column's is
/// `from:*`. The search card's box is in the toolbar, out of the card's
/// reach, so the row above its results takes the drop.
test("a column dropped on the search bar keeps the rows with a value in it", async ({
  page,
  request,
}) => {
  // Counted off every row, not asked of the term under test.
  const all = (
    (await (await request.get("/applet/unified_document_index/search?q=&limit=100000")).json()) as {
      rows: { author: string; author_handle: string | null }[];
    }
  ).rows;
  const withAuthor = all.filter((r) => r.author !== "" || r.author_handle).length;
  expect(withAuthor, "some rows have an author").toBeGreaterThan(0);
  expect(withAuthor, "some rows have none").toBeLessThan(all.length);

  await openGrid(page);
  const searchRow = page.locator(".view-row");
  await searchHeader(page, "author_ref").dragTo(searchRow);
  await expect(page.getByTestId("search-input")).toHaveAttribute("data-query", "from:*");
  await expect(page.locator(".grid-column .status")).toContainText(`(of ${withAuthor})`);
  // The column is still in the grid.
  await expect(searchHeader(page, "author_ref")).toBeVisible();

  // A column the search has no term for says so, and the query stays.
  await searchHeader(page, "snippet").dragTo(searchRow);
  await expect(page.locator(".datalib-toast", { hasText: "cannot filter by" })).toBeVisible();
  await expect(page.getByTestId("search-input")).toHaveAttribute("data-query", "from:*");
});

/// A column pinned from its header menu is still pinned after a reload:
/// the card keeps the pin in its layout, and carries it across the
/// grid's own re-layouts, which rebuild columns without it. Under e2e the
/// layout is kept in this tab's session storage, which a reload keeps.
test("a column pinned from its header menu stays pinned across a reload", async ({ page }) => {
  const source = searchHeader(page, "source_ref");
  // The columns the saved layout pins, read off every card's `cols`.
  const savedPins = () =>
    page.evaluate(() => {
      const tree = sessionStorage.getItem("datalib-layout") ?? "";
      return [...tree.matchAll(/cols(?:=|%3D)([A-Za-z0-9_-]+)/g)].flatMap((m) => {
        const json = decodeURIComponent(escape(atob(m[1].replace(/-/g, "+").replace(/_/g, "/"))));
        const layout = JSON.parse(json) as { cols: { id: string; pinned?: string }[] };
        return layout.cols.filter((c) => c.pinned).map((c) => `${c.id}:${c.pinned}`);
      });
    });

  await openGrid(page);
  await source.hover();
  await source.locator(".slick-header-menu-button").click();
  await page.locator(".slick-header-menu .slick-menu-item", { hasText: "Column Pinning" }).hover();
  await page.locator(".slick-menu-item", { hasText: "Pin to Left" }).click();
  await expect(source).toHaveClass(/\bslick-column-pinned-left\b/);
  await expect.poll(savedPins).toEqual(["source_ref:left"]);

  await page.reload();
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  await expect(source).toHaveClass(/\bslick-column-pinned-left\b/);
});

/// Every column but Score and Contents has a search key, so a cell's
/// right-click can keep only its value: here Touched, a stamp.
test("a cell's right-click keeps only its value, in any column", async ({ page }) => {
  await openGrid(page);
  const uuid = await firstRowUuid(page);
  const touched = await page.evaluate(
    (u) =>
      (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().find((r) => r.uuid === u)!
        .touched_at as string,
    uuid,
  );
  await actOnRowByUuid(
    page,
    uuid,
    (row) => row.locator('[col-id="touched_at"]').click({ button: "right", timeout: 3_000 }),
    "touched_at",
  );
  await searchMenuItem(page, /Keep only Touched=/).click();
  await expect(page.getByTestId("search-input")).toHaveAttribute(
    "data-query",
    // A term splits at its first colon, so the stamp's own need no quotes.
    `touched_at:${touched}`,
  );
  await gridSettled(page);
  const held = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().map((r) => r.touched_at),
  );
  expect(held.length).toBeGreaterThan(0);
  expect(new Set(held)).toEqual(new Set([touched]));
});

/// The top row's right-click asks for the page above it, and the grid
/// scrolls to hold that row in place when the page lands. The menu
/// closed on any scroll of the grid, so a page that landed after the
/// menu opened — as it does on a loaded runner — closed it before Keep
/// only could be clicked. The page is held here until the menu is open.
test("a page landing while the menu is open leaves it open", async ({ page }) => {
  const held: Route[] = [];
  let holding = true;
  await page.route("**/applet/unified_document_index/search?**", (r) => {
    const offset = Number(new URL(r.request().url()).searchParams.get("offset") ?? 0);
    if (holding && offset > 0) held.push(r);
    else void r.continue();
  });
  await openGrid(page);
  const uuid = await firstRowUuid(page);
  await actOnRowByUuid(
    page,
    uuid,
    (row) => row.locator('[col-id="touched_at"]').click({ button: "right", timeout: 3_000 }),
    "touched_at",
  );
  await expect(page.locator(SEARCH_MENU)).toBeVisible();
  expect(held.length, "the page above the top row was asked for").toBeGreaterThan(0);
  holding = false;
  for (const r of held.splice(0)) await r.continue();
  await gridSettled(page);
  await expect(searchMenuItem(page, /Keep only Touched=/)).toBeVisible();
});
