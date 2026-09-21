// The Pipeline table on /data_sources (the sources card) actually paints.

import { test, expect } from "@playwright/test";
import { TABLE_ROWS, expectGridPainted } from "./grid-helpers";

test("the Pipeline table paints at full height", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();

  // Rows are bound. This stayed true throughout the bug, so it is the
  // precondition — not the check.
  await expect(page.locator(TABLE_ROWS)).not.toHaveCount(0, { timeout: 10_000 });

  // The check: the grid occupies real space on screen.
  await expectGridPainted(page.locator(".tg-grid .slickgrid-container"), "Manager2 grid");

  // Headers too — they live in their own header viewport, and a
  // container that collapses takes both with it.
  await expect(page.locator('.tg-grid .slick-header-column[col-id="name"]')).toBeVisible();
});
