// A pasted uuid is a filter on the grid index's search terms, which the
// fixture's own `unified_grid_and_search_index` step wrote, not words to
// rank: the rows naming it are listed, unscored, and its own row comes
// back first, since no newer row names it.

import { test, expect, type Page } from "@playwright/test";
import { EVERY_ROW, SEARCH_ROWS, searchAndSettle, type GridApi } from "./grid-helpers";

const firstUuid = (page: Page) =>
  page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(0),
  ) as Promise<string | null>;

test("a pasted uuid is a filter: its row comes back first", async ({ page, request }) => {
  const all = await request.get("/applet/unified_grid_and_search_index/search?q=&limit=50");
  expect(all.ok()).toBeTruthy();
  const { rows } = (await all.json()) as { rows: { uuid: string }[] };
  const uuid = rows.find((r) => /^[0-9a-f]{8}-[0-9a-f]{4}-/.test(r.uuid))?.uuid;
  expect(uuid, "the fixture has a row with a uuid").toBeDefined();

  const found = await request.get(
    `/applet/unified_grid_and_search_index/search?q=${uuid}&limit=10`,
  );
  const answer = (await found.json()) as {
    rows: { uuid: string; snippet: string; score: number | null }[];
    query_echo: { search_error?: string | null };
  };
  expect(answer.rows[0]?.uuid).toBe(uuid);
  expect(answer.rows[0]?.score ?? null, "a filter ranks nothing").toBeNull();
  expect(answer.query_echo.search_error ?? null).toBeNull();

  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await searchAndSettle(page, uuid!);
  await expect.poll(() => firstUuid(page)).toBe(uuid);
});
