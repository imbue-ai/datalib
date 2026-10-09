// A pasted uuid is answered from the grid's search terms file, which the
// fixture's own `grid_index` step wrote, not from qmd: the row comes back
// first, saying it matched as its own id.

import { test, expect, type Page } from "@playwright/test";
import { EVERY_ROW, SEARCH_ROWS, searchAndSettle, type GridApi } from "./grid-helpers";

const firstUuid = (page: Page) =>
  page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(0),
  ) as Promise<string | null>;

test("a pasted uuid finds its row from the terms, first", async ({ page, request }) => {
  const all = await request.get("/applet/unified_index/search?q=&limit=50");
  expect(all.ok()).toBeTruthy();
  const { rows } = (await all.json()) as { rows: { uuid: string }[] };
  const uuid = rows.find((r) => /^[0-9a-f]{8}-[0-9a-f]{4}-/.test(r.uuid))?.uuid;
  expect(uuid, "the fixture has a row with a uuid").toBeDefined();

  const found = await request.get(`/applet/unified_index/search?q=${uuid}&limit=10`);
  const answer = (await found.json()) as {
    rows: { uuid: string; snippet: string; score: number | null }[];
    query_echo: { qmd_error?: string | null };
  };
  expect(answer.rows[0]?.uuid).toBe(uuid);
  expect(answer.rows[0]?.snippet).toBe(`id: ${uuid}`);
  expect(answer.rows[0]?.score).toBe(5);
  expect(answer.query_echo.qmd_error ?? null).toBeNull();

  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await searchAndSettle(page, uuid!);
  await expect.poll(() => firstUuid(page)).toBe(uuid);
});
