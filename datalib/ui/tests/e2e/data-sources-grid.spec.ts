// The Pipeline table on /data_sources (the sources card) actually paints.

import { test, expect, type Locator } from "@playwright/test";
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

/// Hovering a row lights all of it, the pinned Name and the cells that
/// scroll alike. When pinned columns drew each row in two panes, the
/// theme's `:hover` lit only the half under the pointer (#881).
test("hovering a row lights its pinned cells and the rest alike", async ({ page }) => {
  await page.goto("/data_sources");
  const rows = page.locator(TABLE_ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  // The colour a cell shows: its own background, or the first one behind it.
  const shade = (cell: Locator) =>
    cell.evaluate((el) => {
      for (let at: Element | null = el; at; at = at.parentElement) {
        const bg = getComputedStyle(at).backgroundColor;
        if (bg !== "rgba(0, 0, 0, 0)" && bg !== "transparent") return bg;
      }
      return "";
    });
  const row = rows.nth(1);
  const name = row.locator('[col-id="name"]');
  const last = row.locator(".slick-cell").last();
  const resting = await shade(last);

  await name.hover();
  await expect.poll(() => shade(last)).not.toBe(resting);
  const lit = await shade(last);
  expect(await shade(name)).toBe(lit);

  await last.hover();
  await expect.poll(() => shade(name)).toBe(lit);
});
