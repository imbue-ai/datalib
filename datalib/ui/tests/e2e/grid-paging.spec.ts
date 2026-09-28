// The search grid holds a search a page at a time (grid/pagedWindow.ts).
// The fixture has more rows than one page, so opening the grid holds only
// the newest of them, and the rest arrive as it is scrolled.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { SEARCH_ROWS, searchHeader, type GridApi } from "./grid-helpers";

async function searchUuids(request: APIRequestContext, params: string): Promise<string[]> {
  const r = await request.get(`/applet/unified_index/search?q=&${params}`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: { uuid: string }[] }).rows.map((row) => row.uuid);
}

/// The grid's "N rows (of M)": how many it holds, of how many there are.
async function held(page: Page): Promise<{ loaded: number; total: number }> {
  const status = await page.locator(".grid-column .status").first().textContent();
  const m = status!.match(/(\d+) rows \(of (\d+)\)/)!;
  return { loaded: Number(m[1]), total: Number(m[2]) };
}

const api = (page: Page) =>
  page.evaluate(() => {
    const a = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
    const n = a.rows().length;
    return { first: a.uuidAt(0), last: a.uuidAt(n - 1) };
  });

/// Newest first is the default order, shown the other way up: the newest
/// row at the bottom, where the grid opens, and older ones loading above
/// it as the grid is scrolled up, until the oldest is there too.
test("the grid opens on the newest page and loads older rows as it is scrolled up", async ({
  page,
  request,
}) => {
  const newestFirst = await searchUuids(request, "limit=100000");
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });

  const opened = await held(page);
  expect(opened.total).toBe(newestFirst.length);
  expect(opened.loaded, "the grid held every row at once").toBeLessThan(opened.total);
  expect((await api(page)).last).toBe(newestFirst[0]);

  await expect
    .poll(
      async () => {
        await page.evaluate(() =>
          (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.scrollToRow(0),
        );
        return (await held(page)).loaded;
      },
      { timeout: 15_000, message: "scrolling up never loaded the rest" },
    )
    .toBe(opened.total);
  expect((await api(page)).first).toBe(newestFirst[newestFirst.length - 1]);
});

/// A header click orders the whole search, not the rows the grid holds:
/// the oldest row of all comes first, though the page the grid opened on
/// held only the newest.
test("a header sort orders the whole search", async ({ page, request }) => {
  const [oldest] = await searchUuids(request, "limit=1&sort=created_at:asc");
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  const openedOn = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().map((r) => r.uuid),
  );
  expect(openedOn, "the fixture's oldest row is past the first page").not.toContain(oldest);

  await searchHeader(page, "created_at").click();
  await expect.poll(async () => (await api(page)).first).toBe(oldest);
  expect((await held(page)).loaded, "a sort loaded every row").toBeLessThan(
    (await held(page)).total,
  );
});
