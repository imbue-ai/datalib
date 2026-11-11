// What a click, a ⌘-click and the keys select in the search's two
// views, and that a selection outlives a re-sort and another search.
import { test, expect, type Page } from "@playwright/test";
import {
  EVERY_ROW,
  GRID,
  actOnRowByUuid,
  firstRowUuid,
  gridSettled,
  runSearch,
  selectRowByUuid,
  shownCards,
  type GridApi,
} from "./grid-helpers";

const SEARCH_PATH = "/applet/unified_document_index/search";
const viewTab = (page: Page, name: string) =>
  shownCards(page).getByRole("tab", { name, exact: true });
const sortPick = (page: Page) => shownCards(page).getByTestId("search-sort");
const results = (page: Page) =>
  shownCards(page).getByRole("list", { name: "Results" }).locator(".sc-result");

const isSelected = (page: Page, uuid: string) =>
  page.evaluate(
    (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
    uuid,
  );

/// The rows the search answers `q` with, all of them.
async function answer(
  page: Page,
  q: string,
  more: Record<string, string> = {},
): Promise<{ uuid: string; kind: string }[]> {
  const params = new URLSearchParams({ q, limit: "500", ...more });
  const resp = await page.request.get(`${SEARCH_PATH}?${params}`);
  expect(resp.ok()).toBeTruthy();
  return ((await resp.json()) as { rows: { uuid: string; kind: string }[] }).rows;
}

/// Where the keyboard is, through the cards' shadow roots: the class of
/// the focused element and whether it is inside the search grid.
const focusIs = (page: Page) =>
  page.evaluate(() => {
    let a: Element | null = document.activeElement;
    while (a?.shadowRoot?.activeElement) a = a.shadowRoot.activeElement;
    return { cls: String(a?.className ?? ""), inGrid: !!a?.closest(".grid-wrap") };
  });

async function openTable(page: Page) {
  await page.goto(GRID);
  await viewTab(page, "Table").click();
  await firstRowUuid(page);
}

/// The regression: a click on a chip (the Source cell's) selected nothing,
/// because the click arrived at the grid already prevented.
test("a click on a chip in a row selects the row", async ({ page }) => {
  await openTable(page);
  const uuid = await firstRowUuid(page);
  await actOnRowByUuid(page, uuid, (row) =>
    row.locator("a.chip").first().click({ timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, uuid), { message: "the chip's row" }).toBe(true);
});

/// A ⌘-click adds a row; a re-sort keeps both, a narrower search keeps
/// the one still in it and drops the other for good, so widening again
/// does not bring it back; a plain click is the whole selection again.
test("a selection survives a re-sort, and a search keeps the rows still in it", async ({
  page,
}) => {
  const q = "source:tng_calendar is:document";
  const all = await answer(page, q);
  // Every row on the first page, so a re-sort cannot push one out of
  // what the grid holds.
  expect(all.length, "the fixture's calendar must have a few documents").toBeGreaterThan(2);
  expect(all.length, "and fit in the grid's first page").toBeLessThan(50);
  // Two rows of different kinds, so a search can leave one of them out.
  const first = all[0];
  const other = all.find((r) => r.kind !== first.kind);
  expect(other, "the calendar must hold documents of two kinds").toBeTruthy();
  const [a, b] = [first.uuid, other!.uuid];
  const withoutB = `${q} -kind:"${other!.kind}"`;
  const narrowed = (await answer(page, withoutB)).map((r) => r.uuid);
  expect(narrowed, "the narrower search must keep a").toContain(a);
  expect(narrowed, "and leave b out").not.toContain(b);

  await openTable(page);
  await runSearch(page, q);
  await gridSettled(page);
  await selectRowByUuid(page, a);
  await actOnRowByUuid(page, b, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, b), { message: "the ⌘-clicked row" }).toBe(true);
  expect(await isSelected(page, a), "a ⌘-click adds to the selection").toBe(true);

  await sortPick(page).selectOption({ label: "Oldest first" });
  await gridSettled(page);
  await expect.poll(() => isSelected(page, a), { message: "a, after the re-sort" }).toBe(true);
  await expect.poll(() => isSelected(page, b), { message: "b, after the re-sort" }).toBe(true);

  await runSearch(page, withoutB);
  await gridSettled(page);
  await expect.poll(() => isSelected(page, a), { message: "a, in the narrower search" }).toBe(true);

  await runSearch(page, q);
  await gridSettled(page);
  await expect.poll(() => isSelected(page, a), { message: "a, in the wider search" }).toBe(true);
  expect(await isSelected(page, b), "b, left out by the narrower search, stays out").toBe(false);

  await actOnRowByUuid(page, b, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, b), { message: "b, ⌘-clicked again" }).toBe(true);
  await actOnRowByUuid(page, a, (row) => row.click({ position: { x: 40, y: 10 }, timeout: 3_000 }));
  await expect.poll(() => isSelected(page, b), { message: "a plain click replaces" }).toBe(false);
  expect(await isSelected(page, a)).toBe(true);
});

/// With more rows than the grid loads at once, whether a selected row is
/// still in a narrower search is the server's to say (`among`): a row the
/// search holds past the loaded page is kept, one it leaves out is not.
test("a search of many rows keeps the selected rows it holds past the loaded page", async ({
  page,
}) => {
  const newest = await answer(page, "", { sort: "touched_at:desc" });
  const a = newest[0];
  const bAt = newest.findIndex((r) => r.kind !== a.kind);
  expect(bAt, "the newest rows must be of two kinds").toBeGreaterThan(0);
  expect(bAt, "both on the grid's first page").toBeLessThan(200);
  const b = newest[bAt];
  const narrower = `-kind:"${b.kind}"`;
  const narrowedOldest = await answer(page, narrower, { sort: "touched_at:asc", limit: "100000" });
  expect(narrowedOldest.length, "the narrower search must pass one page").toBeGreaterThan(200);
  const at = narrowedOldest.findIndex((r) => r.uuid === a.uuid);
  expect(at, "the narrower search holds a past its first page").toBeGreaterThanOrEqual(200);

  await page.goto(EVERY_ROW);
  await viewTab(page, "Table").click();
  await firstRowUuid(page);
  await gridSettled(page);
  await selectRowByUuid(page, a.uuid);
  await actOnRowByUuid(page, b.uuid, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, b.uuid), { message: "the ⌘-clicked row" }).toBe(true);
  await sortPick(page).selectOption({ label: "Oldest first" });
  await gridSettled(page);

  // The question is asked after the rows land; wait for its answer before
  // the next search, which would make it moot.
  const asked = page.waitForResponse((r) => r.url().includes("among="));
  await runSearch(page, narrower);
  await gridSettled(page);
  await asked;

  await runSearch(page, "");
  await gridSettled(page);
  await page.evaluate(
    async (ids) => {
      const api = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
      for (const id of ids) await api.seek(id);
    },
    [a.uuid, b.uuid],
  );
  await expect
    .poll(() => isSelected(page, a.uuid), { message: "a, held past the page" })
    .toBe(true);
  expect(await isSelected(page, b.uuid), "b, left out by the narrower search").toBe(false);
});

/// The grid's own keeping held on to a row a search hid, even after a
/// plain click asked for one row alone: the hidden row came back
/// selected beside it when the search widened again.
test("a plain click forgets a selected row the search had hidden", async ({ page }) => {
  const q = "source:tng_calendar is:document";
  const all = await answer(page, q);
  const first = all[0];
  const other = all.find((r) => r.kind !== first.kind);
  const third = all.find((r) => r.kind === first.kind && r.uuid !== first.uuid);
  expect(other && third, "the calendar must hold two kinds, one of them twice").toBeTruthy();
  const narrower = `${q} -kind:"${other!.kind}"`;
  expect((await answer(page, narrower)).map((r) => r.uuid)).not.toContain(other!.uuid);

  await openTable(page);
  await runSearch(page, q);
  await gridSettled(page);
  await selectRowByUuid(page, first.uuid);
  await actOnRowByUuid(page, other!.uuid, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, other!.uuid), { message: "⌘-clicked" }).toBe(true);

  await runSearch(page, narrower);
  await gridSettled(page);
  await actOnRowByUuid(page, third!.uuid, (row) =>
    row.click({ position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, third!.uuid), { message: "plain-clicked" }).toBe(true);

  await runSearch(page, q);
  await gridSettled(page);
  await expect.poll(() => isSelected(page, third!.uuid), { message: "the one row" }).toBe(true);
  expect(await isSelected(page, other!.uuid), "the hidden row stays forgotten").toBe(false);
  expect(await isSelected(page, first.uuid)).toBe(false);
});

/// The regression the grid's own bookkeeping leaves: a selected row
/// that a re-sort moves past the rows loaded so far was dropped, and
/// not selected when it loaded. A page is 200 rows.
test("a selected row a re-sort pushes past the loaded rows is selected when it loads", async ({
  page,
}) => {
  const newest = await answer(page, "", { sort: "touched_at:desc" });
  const [a, b] = newest.map((r) => r.uuid);
  const oldestPage = (await answer(page, "", { sort: "touched_at:asc", limit: "200" })).map(
    (r) => r.uuid,
  );
  expect(oldestPage, "the newest rows must be past the oldest-first page").not.toContain(a);
  expect(oldestPage).not.toContain(b);

  await page.goto(EVERY_ROW);
  await viewTab(page, "Table").click();
  await firstRowUuid(page);
  await gridSettled(page);
  await selectRowByUuid(page, a);
  await actOnRowByUuid(page, b, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, b), { message: "the ⌘-clicked row" }).toBe(true);

  await sortPick(page).selectOption({ label: "Oldest first" });
  await gridSettled(page);
  await page.evaluate(
    async (ids) => {
      const api = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
      for (const id of ids) await api.seek(id);
    },
    [a, b],
  );
  await expect.poll(() => isSelected(page, a), { message: "a, once loaded" }).toBe(true);
  await expect.poll(() => isSelected(page, b), { message: "b, once loaded" }).toBe(true);
});

/// Tab used to walk the selected row's cells; it leaves the table now,
/// and Tab back in lands on the selected row.
test("Tab leaves the table's rows, and comes back to the selected one", async ({ page }) => {
  await openTable(page);
  const uuid = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(2)!,
  );
  const inGrid = async () => (await focusIs(page)).inGrid;
  await selectRowByUuid(page, uuid);
  await page.keyboard.press("Tab");
  await expect.poll(inGrid, { message: "Tab out of the grid" }).toBe(false);

  await actOnRowByUuid(page, uuid, (row) =>
    row.click({ position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(inGrid, { message: "the click puts the keyboard in the grid" }).toBe(true);
  await page.keyboard.press("Shift+Tab");
  await expect.poll(inGrid, { message: "Shift+Tab out of the grid" }).toBe(false);
  await page.keyboard.press("Tab");
  await expect.poll(inGrid, { message: "Tab back in" }).toBe(true);
  expect(await isSelected(page, uuid), "on the row selected before").toBe(true);
  await page.keyboard.press("ArrowDown");
  await expect.poll(() => isSelected(page, uuid), { message: "↓ moved on" }).toBe(false);
});

/// With several rows selected, ↓ and ↑ move among them alone and keep
/// them all selected, as Lightroom's filmstrip does; the grid's own ↓
/// selected the next row alone.
test("the arrows move among several selected rows and keep them", async ({ page }) => {
  await openTable(page);
  await gridSettled(page);
  const uuidAt = (row: number) =>
    page.evaluate(
      (r) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(r)!,
      row,
    );
  const activeUuid = () =>
    page.evaluate(() => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.activeUuid());
  const [a, b] = [await uuidAt(1), await uuidAt(4)];
  const between = await uuidAt(2);

  await selectRowByUuid(page, a);
  await actOnRowByUuid(page, b, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect.poll(() => isSelected(page, b), { message: "the ⌘-clicked row" }).toBe(true);
  await expect.poll(activeUuid, { message: "the ⌘-click makes b active" }).toBe(b);

  await page.keyboard.press("ArrowUp");
  await expect.poll(activeUuid, { message: "↑ skips the rows between" }).toBe(a);
  expect(await isSelected(page, b), "and keeps b").toBe(true);
  expect(await isSelected(page, between)).toBe(false);

  await page.keyboard.press("ArrowUp");
  await expect.poll(activeUuid, { message: "the first stays put" }).toBe(a);
  await page.keyboard.press("ArrowDown");
  await expect.poll(activeUuid, { message: "↓ back to b" }).toBe(b);
  expect(await isSelected(page, a), "a is still selected").toBe(true);
});

/// The list's results were buttons the arrows did not move between, and
/// every one of them was a Tab stop.
test("the arrows move the list's pick, and Tab leaves the list", async ({ page }) => {
  await page.goto(GRID);
  await viewTab(page, "List and preview").click();
  await expect(results(page).nth(2)).toBeVisible({ timeout: 10_000 });
  await results(page).first().click();
  await expect(results(page).first()).toHaveClass(/is-picked/);

  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await expect(results(page).nth(2)).toHaveClass(/is-picked/);
  await expect(results(page).nth(2)).toBeFocused();
  await page.keyboard.press("Home");
  await expect(results(page).first()).toHaveClass(/is-picked/);

  await page.keyboard.press("Tab");
  await expect
    .poll(async () => (await focusIs(page)).cls, { message: "past the results" })
    .not.toContain("sc-result");
});
