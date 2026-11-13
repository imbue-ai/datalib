// A Manage row's problem counts, after its name, open in the search grid over the index's
// `problems` table: exactly the rows they count, by the query the row
// carries, as text in its search bar; and a cell's right-click narrows
// it the way the search's does.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  cardOf,
  expandGroup,
  gridSettled,
  nameCell,
  SEARCH_ROWS,
  searchMenuItem,
  type GridApi,
} from "./grid-helpers";

type Problem = {
  problem_uuid: string;
  source_id: string;
  severity: string;
  markdown_uuid: string | null;
};

async function problems(request: APIRequestContext, q: string): Promise<Problem[]> {
  const r = await request.get(
    `/applet/unified_document_index/problems?q=${encodeURIComponent(q)}&limit=2000`,
  );
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: Problem[] }).rows;
}

const held = (page: Page) =>
  page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows(),
  ) as Promise<Problem[]>;

type CountedRow = { key: string; problems: { kind: string }[]; problems_query: string | null };

/// A Manage row as the server sends it: its counts, and the query for
/// exactly the rows they count.
async function manageRow(request: APIRequestContext, key: string): Promise<CountedRow> {
  const r = await request.get("/api/manage/rows");
  expect(r.ok()).toBe(true);
  const row = ((await r.json()) as { rows: CountedRow[] }).rows.find((x) => x.key === key);
  expect(row, `a Manage row ${key}`).toBeDefined();
  return row!;
}

/// The red and yellow numbers a set of rows makes.
function numbers(rows: Problem[]): string[] {
  const n = (severity: string) => rows.filter((p) => p.severity === severity).length;
  return [n("error"), n("warning")].filter((x) => x > 0).map(String);
}

/// The counts after a row's name — a group's, or with `key` a step's.
const counts = (page: Page, key: string) => nameCell(page, key).locator(".tg-badges");
const red = (page: Page, key: string) =>
  counts(page, key).locator(".tg-chip-error, .tg-chip-warning");

/// The source whose render found the most errors and warnings in the
/// fixture, and the query its group's counts open with.
async function busiest(request: APIRequestContext): Promise<[string, string]> {
  const found = await problems(request, "-stage:fetch -doc:render_store -severity:info muted:no");
  expect(found.length, "the fixture has problems a render found").toBeGreaterThan(0);
  const by = new Map<string, number>();
  for (const p of found) by.set(p.source_id, (by.get(p.source_id) ?? 0) + 1);
  const [source] = [...by.entries()].sort((a, b) => b[1] - a[1])[0];
  const query = (await manageRow(request, `group:${source}`)).problems_query;
  expect(query, `group:${source} counts some`).not.toBeNull();
  return [source, query!];
}

/// The number a double-click lands on is the list it opens: the counts
/// come from the index's rows (covered in `manage_rows.rs`), and the
/// query that opens is the one that counted them.
test("a source's problems open in the grid, its filter in the search bar", async ({
  page,
  request,
}) => {
  const [source, query] = await busiest(request);
  const theirs = await problems(request, query);
  await page.goto("/data_sources");
  const key = `group:${source}`;
  await expect(red(page, key)).toHaveText(numbers(theirs));
  await red(page, key).first().dblclick();

  await expect(page.getByTestId("grid-search-input")).toHaveAttribute("data-query", query);
  // The badges are inside the Name cell, which a double-click would
  // otherwise open for renaming.
  await expect(nameCell(page, key).locator("input")).toHaveCount(0);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  const rows = await held(page);
  expect(rows.map((r) => r.problem_uuid).sort()).toEqual(theirs.map((p) => p.problem_uuid).sort());
});

/// A step's Name is its chip, and its counts stay beside the chip: they
/// were dropped once when the chip replaced the whole cell, which no test
/// caught because only a group row's counts were checked. A double-click
/// on them still opens the step's problems, not the chip's card.
test("a step's counts sit beside its chip, and open its problems", async ({ page, request }) => {
  const [source] = await busiest(request);
  const step = `${source}/render_markdown`;
  const query = (await manageRow(request, step)).problems_query;
  expect(query).not.toBeNull();
  await page.goto("/data_sources");
  await expandGroup(page, source);
  const cell = nameCell(page, step);
  await expect(cell.locator(`a.chip[data-entity="datalib:step/${step}"]`)).toBeVisible();
  await expect(red(page, step)).toHaveText(numbers(await problems(request, query!)));
  await red(page, step).first().dblclick();
  await expect(page.getByTestId("grid-search-input")).toHaveAttribute("data-query", query!);
});

/// In a short window too: the menu is taller than the room above or below
/// a row in the middle of it, and must still be reachable end to end.
test("a severity cell's right-click keeps only its severity", async ({ page, request }) => {
  const [source, query] = await busiest(request);
  const theirs = await problems(request, query);
  await page.goto("/data_sources");
  // Opened at full height: in the short window the Manage grid scrolls
  // to reach a row low in it, and a double-click lands on whichever row
  // it redrew there.
  await red(page, `group:${source}`).first().dblclick();
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  await page.setViewportSize({ width: 1280, height: 480 });
  await gridSettled(page);

  const target = theirs[0];
  await actOnRowByUuid(
    page,
    target.problem_uuid,
    (row) => row.locator('[col-id="severity_chip"]').click({ button: "right", timeout: 3_000 }),
    "severity_chip",
  );
  await searchMenuItem(page, new RegExp(`Keep only Severity=${target.severity}`)).click();
  await expect(page.getByTestId("grid-search-input")).toHaveAttribute(
    "data-query",
    `${query} severity:${target.severity}`,
  );
  await gridSettled(page);
  const kept = await held(page);
  expect(kept.length).toBe(theirs.filter((p) => p.severity === target.severity).length);
  expect(new Set(kept.map((r) => r.severity))).toEqual(new Set([target.severity]));
});

/// A Document cell is the document's chip, by its title rather than its
/// uuid, and a double-click on it opens the document beside the grid,
/// in this window.
test("a Document chip names its document and opens it beside the grid", async ({
  page,
  request,
}) => {
  const counted = await problems(request, "-doc:render_store -severity:info muted:no");
  const target = counted.find((p) => p.markdown_uuid);
  expect(target, "the fixture has a counted problem on a document").toBeDefined();
  const { source_id: source, problem_uuid, markdown_uuid } = target!;
  await page.goto("/data_sources");
  await red(page, `group:${source}`).first().dblclick();
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await gridSettled(page);

  await actOnRowByUuid(
    page,
    problem_uuid,
    async (row) => {
      const chip = row.locator('[col-id="document_ref"] a.chip[data-doc]');
      await expect(chip).toHaveAttribute("data-doc", `datalib:doc/${markdown_uuid}`, {
        timeout: 3_000,
      });
      await expect(chip).not.toContainText(markdown_uuid!);
      await chip.dblclick({ timeout: 3_000 });
    },
    "document_ref",
  );
  await expect(cardOf(page, `documentView(${JSON.stringify(markdown_uuid)}`)).toBeVisible({
    timeout: 10_000,
  });
  expect(page.context().pages()).toHaveLength(1);
});
