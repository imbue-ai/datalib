// The search grid inside its card: it follows the card's width when
// the card is dragged wider or narrower, its filter row narrows the
// rows, and a group header row is a thing to fold, not a row to open.
//
// The first is the one that was quietly wrong: the grid's resizer was
// told to watch the very box it sizes, so once it had set a width the
// box stopped following the card. Both directions are asserted —
// growing worked even then.

import { test, expect, type Page } from "@playwright/test";
import { everyRowLoaded, SEARCH_ROWS, searchGrid, type GridApi } from "./grid-helpers";

async function openGrid(page: Page) {
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
}

/// Drag the first card's right edge by `dx` pixels, the way a person
/// does.
async function dragCardEdge(page: Page, dx: number) {
  const handle = page.locator(".miller-col-resize").first();
  const box = (await handle.boundingBox())!;
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + dx / 2, y);
  await page.mouse.move(x + dx, y);
  await page.mouse.up();
}

const gridWidth = (page: Page) =>
  searchGrid(page)
    .first()
    .evaluate((el) => el.getBoundingClientRect().width);

test("the grid follows the card's width, wider and narrower", async ({ page }) => {
  await openGrid(page);
  const before = await gridWidth(page);

  await dragCardEdge(page, 240);
  // The resizer debounces what it observes; poll rather than read once.
  await expect.poll(() => gridWidth(page), { timeout: 5_000 }).toBeGreaterThan(before + 200);

  await dragCardEdge(page, -240);
  await expect.poll(() => gridWidth(page), { timeout: 5_000 }).toBeLessThan(before + 40);
});

test("the filter row narrows the rows to the typed value", async ({ page }) => {
  await openGrid(page);
  const rowCount = () =>
    searchGrid(page)
      .first()
      .evaluate((el) => Number(el.getAttribute("aria-rowcount")));
  const all = await rowCount();
  expect(all).toBeGreaterThan(30);

  // Key by key: the filter listens for keyup, as a person's typing
  // produces it, and `fill` would set the value without one.
  const sourceFilter = page.locator(".grid-box input.filter-source_ref");
  await sourceFilter.click();
  await sourceFilter.pressSequentially("slack");
  await expect.poll(rowCount, { timeout: 5_000 }).toBeLessThan(all);
  await expect.poll(rowCount).toBeGreaterThan(0);

  // Every row left names the typed value in its Source cell — read off
  // the grid's filtered rows, not the few painted. The filter is a
  // substring match, so the fixture's `slack-diff` group stays too.
  const sources = await page.evaluate(() => [
    ...new Set(
      (window as unknown as { __fwGridApi: GridApi }).__fwGridApi
        .filteredRows()
        .map((r) => (r.source_ref as { label: string }).label),
    ),
  ]);
  expect(sources).toContain("slack");
  expect(sources.filter((s) => !s.includes("slack"))).toEqual([]);
});

test("clicking a group header folds the group and opens nothing", async ({ page }) => {
  await openGrid(page);
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.groupBy(["kind"]),
  );
  // Grouped, the grid loads the whole search; the groups settle once it has.
  await everyRowLoaded(page);
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.scrollToRow(0),
  );
  const group = page.locator(".grid-box .slick-row.slick-group").first();
  await expect(group).toBeVisible();
  const title = (await group.textContent())!.trim();
  expect(title).toMatch(/^Type: .+ \(\d+\)$/);
  // The title is where the reader looks for it: at the start of the
  // row, not pushed off its far end by the first column's alignment.
  const titleBox = (await group.locator(".slick-group-title").boundingBox())!;
  const gridBox = (await searchGrid(page).first().boundingBox())!;
  expect(titleBox.x).toBeLessThan(gridBox.x + 100);

  await group.locator(".slick-group-toggle").click();
  await expect(group.locator(".slick-group-toggle")).toHaveAttribute("aria-expanded", "false");
  // No document card came of it.
  await expect(page.locator(".miller-col")).toHaveCount(1);
});
