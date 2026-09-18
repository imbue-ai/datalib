// The run-log panel: the Manage screen's "Server log" opens the app
// server's own lines in a grid, a right-click on a cell narrows the
// query to that cell's value (and clears it again), and the bar above
// the grid groups the lines by a column.
//
// The grid is built straight on the vanilla SlickGrid bundle, like the
// cards' grids; this is the one place its menu, grouping bar and query
// round-trip are exercised end to end.

import { test, expect, type Page } from "@playwright/test";
import { menuEntry } from "./grid-helpers";

const ROWS = ".rl-grid .slick-row:not(.slick-group)";

async function openServerLog(page: Page) {
  await page.goto("/sources2");
  await page.getByRole("button", { name: "Server log" }).click();
  const dialog = page.getByRole("dialog", { name: "Server log" });
  await expect(dialog).toBeVisible();
  await expect(dialog.locator(ROWS).first()).toBeVisible({ timeout: 10_000 });
  return dialog;
}

const lineCount = (page: Page) =>
  page
    .locator(".rl-count")
    .evaluate((el) => Number(/(\d+) line/.exec(el.textContent ?? "")?.[1] ?? NaN));

test("a cell's right-click keeps only its value, and the query clears again", async ({ page }) => {
  const dialog = await openServerLog(page);
  const query = dialog.locator(".rl-search");
  await expect(query).toHaveValue("process:http");
  const all = await lineCount(page);
  expect(all).toBeGreaterThan(1);

  // The server's boot lines all come from its main thread but one; the
  // menu names the value under the click.
  const mainCell = dialog.locator(`${ROWS} .slick-cell[col-id="thread"]`).filter({ hasText: /^main$/ }).first();
  await mainCell.click({ button: "right" });
  await expect(menuEntry(page, "Keep only Thread=main")).toBeVisible();
  await expect(menuEntry(page, "Exclude all Thread=main")).toBeVisible();
  await menuEntry(page, "Keep only Thread=main").click();

  await expect(query).toHaveValue("process:http thread:main");
  await expect.poll(() => lineCount(page)).toBeLessThan(all);
  const threads = await dialog.locator(`${ROWS} .slick-cell[col-id="thread"]`).allTextContents();
  expect(new Set(threads.map((t) => t.trim()))).toEqual(new Set(["main"]));

  await dialog.locator(ROWS).first().click({ button: "right" });
  await menuEntry(page, "Clear the query").click();
  await expect(query).toHaveValue("");
  // With no query at all, every line in the store — at least the
  // server's own.
  await expect.poll(() => lineCount(page)).toBeGreaterThanOrEqual(all);
});

// Grouping goes through the panel's `__fwRunLogApi.groupBy`, which
// calls the plugin's own `setDroppedGroups` — the same thing its drop
// handler calls, and how the Explore grid's spec groups too. The drag
// itself is SortableJS's native drag-and-drop, and a drag dispatched by
// hand died inside it on CI's loaded runners at more than one point
// (the header never entering the bar; the drop never ending the drag),
// through three rewrites. What is ours — the columns declared
// groupable, the placeholder, the group row's text, the toggle — is
// what this checks.
test("grouped by a column, the lines fold under group rows", async ({ page }) => {
  const dialog = await openServerLog(page);
  const bar = dialog.locator(".slick-preheader-panel .slick-dropzone");
  await expect(bar).toContainText("Drag a column here");
  await expect(dialog.locator(".slick-group-toggle-all")).toBeHidden();

  await page.evaluate(() =>
    (window as unknown as { __fwRunLogApi: { groupBy: (ids: string[]) => void } }).__fwRunLogApi.groupBy(["level"]),
  );

  // A chip for the column takes the placeholder's place in the bar…
  await expect(bar.locator(".slick-dropped-grouping")).toContainText("Level");
  await expect(bar.locator(".slick-draggable-dropzone-placeholder")).toBeHidden();
  // …and the lines sit under group rows that say what they share and
  // how many there are.
  const group = dialog.locator(".rl-grid .slick-row.slick-group").first();
  await expect(group).toBeVisible();
  await expect(group).toHaveText(/^Level: \w+ \(\d+\)$/);
  await expect(dialog.locator(".slick-group-toggle-all")).toContainText("Expand / collapse all");
});
