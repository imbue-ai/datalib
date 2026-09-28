// A Manage row's problem counts, after its name, open in the search grid over the index's
// `problems` table: the filter it opens with is text in its search bar,
// and a cell's right-click narrows it the way the search's does.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  gridSettled,
  nameCell,
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

/// Give a group row counts after its name, whatever the served root's
/// run store holds: the counts come from metrics a render run reports
/// (covered in `manage_rows.rs`), and this spec is about where a
/// double-click on them leads.
async function withCounts(page: Page, source: string) {
  await page.route("**/api/manage/rows**", async (route) => {
    const response = await route.fetch();
    const body = (await response.json()) as { rows: { key: string; problems: unknown[] }[] };
    for (const r of body.rows) {
      if (r.key === `group:${source}`) {
        r.problems = [
          { kind: "error", text: "2", title: "2 errors" },
          { kind: "warning", text: "5", title: "5 warnings" },
        ];
      }
    }
    await route.fulfill({ response, json: body });
  });
}

/// The counts after a row's name.
const counts = (page: Page, source: string) =>
  nameCell(page, `group:${source}`).locator(".tg-badges");

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
  await withCounts(page, source);
  await page.goto("/data_sources");
  await expect(counts(page, source).locator(".tg-badge")).toHaveText(["2", "5"]);
  await counts(page, source).locator(".tg-chip-warning").dblclick();

  await expect(page.getByTestId("search-input")).toHaveValue(`source_id:${source}`);
  // The badges are inside the Name cell, which a double-click would
  // otherwise open for renaming.
  await expect(nameCell(page, `group:${source}`).locator("input")).toHaveCount(0);
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
  await withCounts(page, source);
  await page.goto("/data_sources");
  await counts(page, source).dblclick();
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
