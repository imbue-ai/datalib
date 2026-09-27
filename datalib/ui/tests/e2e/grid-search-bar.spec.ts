// The search grid's order and filters all go to the server: a header
// click, and a shift-click to add a column, sort the whole search; a
// column dropped on the search bar becomes a term there.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  firstRowUuid,
  gridSettled,
  SEARCH_ROWS,
  searchHeader,
  searchMenuItem,
  type GridApi,
} from "./grid-helpers";

async function searchUuids(request: APIRequestContext, params: string): Promise<string[]> {
  const r = await request.get(`/applet/unified_index/search?${params}`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: { uuid: string }[] }).rows.map((row) => row.uuid);
}

async function openGrid(page: Page) {
  await page.goto("/");
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
  const expected = await searchUuids(request, "q=&limit=10&sort=kind:asc,created_at:asc");
  await openGrid(page);
  await searchHeader(page, "kind").click();
  await searchHeader(page, "created_at").click({ modifiers: ["Shift"] });
  await expect.poll(() => topRows(page, 10)).toEqual(expected);
  await gridSettled(page);
  expect(await topRows(page, 10)).toEqual(expected);
});

/// A column dropped on the search bar keeps the rows with a value in it,
/// as a term a person can read and edit: `author:*`.
test("a column dropped on the search bar keeps the rows with a value in it", async ({
  page,
  request,
}) => {
  // Counted off every row, not asked of the term under test.
  const all = (
    (await (await request.get("/applet/unified_index/search?q=&limit=100000")).json()) as {
      rows: { author: string }[];
    }
  ).rows;
  const withAuthor = all.filter((r) => r.author !== "").length;
  expect(withAuthor, "some rows have an author").toBeGreaterThan(0);
  expect(withAuthor, "some rows have none").toBeLessThan(all.length);

  await openGrid(page);
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.dropOnSearch("author"),
  );
  await expect(page.getByTestId("search-input")).toHaveValue("author:*");
  await expect(page.locator(".grid-column .status")).toContainText(`(of ${withAuthor})`);

  // A column the search has no term for says so, and the query stays.
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.dropOnSearch("snippet"),
  );
  await expect(page.locator(".datalib-toast", { hasText: "cannot filter by" })).toBeVisible();
  await expect(page.getByTestId("search-input")).toHaveValue("author:*");
});

/// Every column but Score and Contents has a search key, so a cell's
/// right-click can keep only its value: here Created, which had none
/// while the grid kept its own list of the columns that could.
test("a cell's right-click keeps only its value, in any column", async ({ page }) => {
  await openGrid(page);
  const uuid = await firstRowUuid(page);
  const created = await page.evaluate(
    (u) =>
      (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().find((r) => r.uuid === u)!
        .created_at as string,
    uuid,
  );
  await actOnRowByUuid(
    page,
    uuid,
    (row) => row.locator('[col-id="created_at"]').click({ button: "right", timeout: 3_000 }),
    "created_at",
  );
  await searchMenuItem(page, /Keep only Created=/).click();
  await expect(page.getByTestId("search-input")).toHaveValue(`created_at:"${created}"`);
  await gridSettled(page);
  const held = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().map((r) => r.created_at),
  );
  expect(held.length).toBeGreaterThan(0);
  expect(new Set(held)).toEqual(new Set([created]));
});
