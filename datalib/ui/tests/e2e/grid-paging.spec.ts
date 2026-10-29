// The search grid holds a search a page at a time (grid/pagedWindow.ts).
// The fixture has more rows than one page, so opening the grid holds only
// the newest of them, and the rest arrive as it is scrolled.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { EVERY_ROW, gridSettled, SEARCH_ROWS, searchHeader, type GridApi } from "./grid-helpers";

async function searchUuids(request: APIRequestContext, params: string): Promise<string[]> {
  const r = await request.get(`/applet/unified_document_index/search?q=&${params}`);
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
  await page.goto(EVERY_ROW);
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
  const [oldest] = await searchUuids(request, "limit=1&sort=touched_at:asc");
  await page.goto(EVERY_ROW);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  const openedOn = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().map((r) => r.uuid),
  );
  expect(openedOn, "the fixture's oldest row is past the first page").not.toContain(oldest);

  await searchHeader(page, "touched_at").click();
  await expect.poll(async () => (await api(page)).first).toBe(oldest);
  expect((await held(page)).loaded, "a sort loaded every row").toBeLessThan(
    (await held(page)).total,
  );
});

/// Whether `uuid`'s row is drawn inside the grid's viewport. Its index
/// moves as older rows load above it, so it is asked for each time.
async function inView(page: Page, uuid: string): Promise<boolean> {
  await gridSettled(page);
  const i = await page.evaluate(
    (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rowIndexOf(u),
    uuid,
  );
  if (i === null) return false;
  const row = page.locator(`${SEARCH_ROWS}[data-row="${i}"]`).first();
  if ((await row.count()) === 0) return false;
  const [r, v] = await Promise.all([
    row.boundingBox(),
    page.locator(".grid-box .slick-viewport").first().boundingBox(),
  ]);
  return !!r && !!v && r.y >= v.y - 1 && r.y + r.height <= v.y + v.height + 1;
}

/// The grid opens at now, the newest row that is not still to come (the
/// suite pins now past the fixture, so that is its newest row), and the
/// Now button brings it back after a scroll. An order that is not newest
/// first by a date has no now to go to.
test("the Now button returns to now, and only a newest-first date order has one", async ({
  page,
  request,
}) => {
  const [newest] = await searchUuids(request, "limit=1");
  await page.goto(EVERY_ROW);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  const now = page.getByTestId("grid-now");
  await expect.poll(() => inView(page, newest)).toBe(true);
  await expect(now).toBeVisible();

  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.scrollToRow(0),
  );
  await expect.poll(() => inView(page, newest)).toBe(false);
  await now.click();
  await expect.poll(() => inView(page, newest)).toBe(true);

  await searchHeader(page, "kind").click();
  await expect(now).toBeHidden();
  await searchHeader(page, "touched_at").click();
  await searchHeader(page, "touched_at").click();
  await expect(now, "Touched, newest first, is by a date").toBeVisible();
});
