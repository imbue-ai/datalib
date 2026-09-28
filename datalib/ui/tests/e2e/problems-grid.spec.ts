// A Manage row's problems open in the search grid over the index's
// `problems` table: the filter it opens with is text in its search bar,
// and a cell's right-click narrows it the way the search's does.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  gridSettled,
  groupRow,
  SEARCH_ROWS,
  searchMenuItem,
  type GridApi,
} from "./grid-helpers";

type Problem = { problem_uuid: string; source_id: string; severity: string };

async function problems(request: APIRequestContext, q: string): Promise<Problem[]> {
  const r = await request.get(`/applet/unified_index/problems?q=${encodeURIComponent(q)}`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: Problem[] }).rows;
}

const held = (page: Page) =>
  page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows(),
  ) as Promise<Problem[]>;

/// The source with the most problems in the fixture, and its problems.
async function busiest(request: APIRequestContext): Promise<[string, Problem[]]> {
  const all = await problems(request, "");
  expect(all.length, "the fixture has problems").toBeGreaterThan(0);
  const by = new Map<string, Problem[]>();
  for (const p of all) by.set(p.source_id, [...(by.get(p.source_id) ?? []), p]);
  return [...by.entries()].sort((a, b) => b[1].length - a[1].length)[0];
}

test("a source's problems open in the grid, its filter in the search bar", async ({
  page,
  request,
}) => {
  const [source, theirs] = await busiest(request);
  await page.goto("/data_sources");
  await groupRow(page, source).locator('[col-id="problems"]').dblclick();

  await expect(page.getByTestId("search-input")).toHaveValue(`source_id:${source}`);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  const rows = await held(page);
  expect(rows.map((r) => r.problem_uuid).sort()).toEqual(theirs.map((p) => p.problem_uuid).sort());
});

/// In a short window too: the menu is taller than the room above or below
/// a row in the middle of it, and must still be reachable end to end.
test("a severity cell's right-click keeps only its severity", async ({ page, request }) => {
  const [source, theirs] = await busiest(request);
  await page.setViewportSize({ width: 1280, height: 480 });
  await page.goto("/data_sources");
  await groupRow(page, source).locator('[col-id="problems"]').dblclick();
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);

  const target = theirs[0];
  await actOnRowByUuid(
    page,
    target.problem_uuid,
    (row) => row.locator('[col-id="severity_chip"]').click({ button: "right", timeout: 3_000 }),
    "severity_chip",
  );
  await searchMenuItem(page, new RegExp(`Keep only Severity=${target.severity}`)).click();
  await expect(page.getByTestId("search-input")).toHaveValue(
    `source_id:${source} severity:${target.severity}`,
  );
  await gridSettled(page);
  const kept = await held(page);
  expect(kept.length).toBe(theirs.filter((p) => p.severity === target.severity).length);
  expect(new Set(kept.map((r) => r.severity))).toEqual(new Set([target.severity]));
});
