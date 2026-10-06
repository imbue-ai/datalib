// What a newcomer sees on opening the app: the search grid one row per
// document, Contents beside Source, and a hint that names a source this
// library has. The suite drives the columns layout; the app opens on
// tabs, so the defaults are checked there too.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { GRID, gridSettled, SEARCH_ROWS, type GridApi } from "./grid-helpers";

async function total(request: APIRequestContext, q: string): Promise<number> {
  const r = await request.get(`/applet/unified_index/search?q=${encodeURIComponent(q)}&limit=1`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { total: number }).total;
}

const shownColumns = (page: Page) =>
  page
    .locator(".grid-box .slick-header-column")
    .evaluateAll((cells) =>
      cells.filter((c) => (c as HTMLElement).offsetWidth > 0).map((c) => c.getAttribute("col-id")),
    );

async function expectDefaults(page: Page, request: APIRequestContext) {
  const documents = await total(request, "is:document");
  expect(documents, "the fixture has documents").toBeGreaterThan(0);
  expect(await total(request, ""), "and rows inside them").toBeGreaterThan(documents);

  await expect(page.getByTestId("search-input")).toHaveValue("is:document");
  await expect(page.locator(".grid-column .status")).toContainText(`(of ${documents})`);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  const notDocuments = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi
      .rows()
      .filter((r) => r.is_document !== true),
  );
  expect(notDocuments).toEqual([]);
  expect((await shownColumns(page)).slice(0, 2)).toEqual(["source_ref", "snippet"]);
}

/// Every message and tool call used to be a row of its own, with the
/// text past the middle of the screen.
test("the grid opens one row per document, Contents second", async ({ page, request }) => {
  await page.goto(GRID);
  await expectDefaults(page, request);
  // Named after what it is, not after the term it opens with.
  await expect(page.locator(".ct-main .ct-card-title").first()).toHaveText("Search");
});

/// The default is a term in the search bar, and deleting it shows every
/// row — and stays deleted, rather than coming back on a reload.
test("a cleared default stays cleared", async ({ page, request }) => {
  const every = await total(request, "");
  await page.goto(GRID);
  await expect(page.getByTestId("search-input")).toHaveValue("is:document");
  await page.getByTestId("search-clear").click();
  await expect(page.locator(".grid-column .status")).toContainText(`(of ${every})`);

  await page.reload();
  await expect(page.getByTestId("search-input")).toHaveValue("");
  await expect(page.locator(".grid-column .status")).toContainText(`(of ${every})`);
});

/// The hint used to suggest `source:Slack` whatever the library held.
/// Its example source has to be one a search finds rows in.
test("the empty search bar suggests a source this library has", async ({ page, request }) => {
  await page.goto(GRID);
  await page.getByTestId("search-clear").click();
  const input = page.getByTestId("search-input");
  await expect(input).toHaveAttribute("placeholder", /source_id:/);
  const hint = (await input.getAttribute("placeholder"))!;
  const source = /source_id:(\S+?)[,)]/.exec(hint)?.[1];
  expect(source, hint).toBeTruthy();
  expect(source).not.toBe("datalib");
  expect(await total(request, `source_id:${source}`), hint).toBeGreaterThan(0);
  const year = /after:(\d{4})-01-01/.exec(hint)?.[1];
  expect(year, hint).toBeTruthy();
  expect(await total(request, `source_id:${source} after:${year}-01-01`), hint).toBeGreaterThan(0);
});
