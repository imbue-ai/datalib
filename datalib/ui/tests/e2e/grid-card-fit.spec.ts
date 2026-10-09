// The search grid inside its card: it follows the card's width when
// the card is dragged wider or narrower, its filter row narrows the
// rows, and a group header row is a thing to fold, not a row to open.
//
// The first is the one that was quietly wrong: the grid's resizer was
// told to watch the very box it sizes, so once it had set a width the
// box stopped following the card. Both directions are asserted —
// growing worked even then.

import { test, expect, type Page } from "@playwright/test";
import {
  GRID,
  type GridApi,
  gridSettled,
  SEARCH_ROWS,
  searchGrid,
  shownCards,
} from "./grid-helpers";

async function openGrid(page: Page) {
  await page.goto(GRID);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
}

/// Drag the first card's right edge by `dx` pixels, the way a person
/// does.
async function dragCardEdge(page: Page, dx: number) {
  const handle = page.locator(".ct-main .ct-handle").first();
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

test("clicking a group header folds the group and opens nothing", async ({ page }) => {
  await openGrid(page);
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.groupBy(["kind"]),
  );
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.scrollToRow(0),
  );
  // The groups on screen have read their rows; nothing is still arriving.
  await gridSettled(page);
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
  await expect(shownCards(page)).toHaveCount(1);
});
