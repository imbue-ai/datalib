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
  await expectGridPainted(page.locator(".tg-grid .slickgrid-container"), "sources card grid");

  // Headers too — they live in their own header viewport, and a
  // container that collapses takes both with it.
  await expect(page.locator('.tg-grid .slick-header-column[col-id="name"]')).toBeVisible();
});

/// The pinned Name column draws each row in a pane of its own, and the
/// theme's `:hover` lit only the half under the pointer.
test("hovering either half of a row lights the other", async ({ page }) => {
  await page.goto("/data_sources");
  const rows = page.locator(TABLE_ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  const half = (key: string, pinned: boolean) =>
    page.locator(
      `.tg-grid .slick-row${pinned ? "[data-pinned]" : ":not([data-pinned])"}[data-key="${key}"]`,
    );

  const first = (await rows.first().getAttribute("data-key"))!;
  await half(first, true).hover();
  await expect(half(first, false)).toHaveClass(/\bdatalib-row-hover\b/);

  const second = (await rows.nth(1).getAttribute("data-key"))!;
  await half(second, false).locator(".slick-cell").last().hover();
  await expect(half(second, true)).toHaveClass(/\bdatalib-row-hover\b/);
  await expect(half(first, false)).not.toHaveClass(/\bdatalib-row-hover\b/);
});
