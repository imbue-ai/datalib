import { test, expect } from "@playwright/test";
import {
  GRID,
  contextMenuRowByUuid,
  firstRowUuid,
  openRowByUuid,
  searchMenuItem,
  selectRowByUuid,
} from "./grid-helpers";

// A click on a search grid row only selects it. Its document opens on a
// double-click, Enter, or "Open document" on the row's menu.

/// The regression: selecting a row opened its document beside the grid.
/// The menu opens after the click's handlers have run, so a document the
/// click had opened would be on its way by then.
test("a clicked row is only selected; its menu opens the document", async ({ page }) => {
  await page.goto(GRID);
  const uuid = await firstRowUuid(page);
  await selectRowByUuid(page, uuid);
  await contextMenuRowByUuid(page, uuid);
  await expect(page.locator(".chat-preview")).toHaveCount(0);

  await searchMenuItem(page, "Open document").click();
  await expect(page.locator(".chat-preview")).toBeVisible();
});

test("a double-click on a row opens its document", async ({ page }) => {
  await page.goto(GRID);
  const uuid = await firstRowUuid(page);
  const row = await selectRowByUuid(page, uuid);
  await expect(page.locator(".chat-preview")).toHaveCount(0);
  // Kind, a cell of plain text: a chip in a cell opens what it names.
  await row.locator('[col-id="kind"]').dblclick();
  await expect(page.locator(".chat-preview")).toBeVisible();
});

test("Enter opens the selected row's document", async ({ page }) => {
  await page.goto(GRID);
  await selectRowByUuid(page, await firstRowUuid(page));
  await expect(page.locator(".chat-preview")).toHaveCount(0);
  await page.keyboard.press("Enter");
  await expect(page.locator(".chat-preview")).toBeVisible();
});

test("the row helper opens the row it names", async ({ page }) => {
  await page.goto(GRID);
  await openRowByUuid(page, await firstRowUuid(page));
  await expect(page.locator(".chat-preview")).toBeVisible();
});
