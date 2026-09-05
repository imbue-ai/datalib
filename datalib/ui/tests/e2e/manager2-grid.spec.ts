// The Pipeline table on /sources2 (Manager2View) actually paints.

import { test, expect } from "@playwright/test";
import { expectGridPainted } from "./grid-helpers";

test("the Pipeline table paints at full height", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();

  // Rows are bound. This stayed true throughout the bug, so it is the
  // precondition — not the check.
  await expect(
    page.locator('.ag-grid-scrolling-rows [role="row"]'),
  ).not.toHaveCount(0, { timeout: 10_000 });

  // The check: the grid occupies real space on screen.
  await expectGridPainted(page.locator(".ag-root-wrapper"), "Manager2 grid");

  // Headers too — they live in their own AG Grid viewport, and a
  // container that collapses takes both with it.
  await expect(page.locator('.ag-header-cell[col-id="name"]')).toBeVisible();
});
