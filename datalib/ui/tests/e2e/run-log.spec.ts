// The run-log panel: the Manage screen's "Server log" opens the app
// server's own lines in a grid, a right-click on a cell narrows the
// query to that cell's value (and clears it again), and a header
// dragged into the bar above the grid groups the lines by it.
//
// The grid is built straight on the vanilla SlickGrid bundle, like the
// cards' grids; this is the one place its menu, grouping bar and query
// round-trip are exercised end to end.

import { test, expect, type Locator, type Page } from "@playwright/test";
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

// A header dragged into the grouping bar. The press and the release are
// real, so the header has to be where the pointer lands; the drag events
// between them are dispatched by hand, at whatever is under the bar's
// centre, so the bar has to be there too. Not a real drag, because
// Playwright's WebKit on macOS turns one into a native drag session and,
// under load, loses it: the page sees dragstart and one dragenter, then
// nothing — not even dragend on the release — and no gesture ends a
// drag the browser has forgotten. Chromium and WebKit on Linux drive a
// real drag fine; this is the same on all three.
async function dragHeaderInto(page: Page, header: Locator, bar: Locator) {
  const from = (await header.boundingBox())!;
  const to = (await bar.boundingBox())!;
  const dataTransfer = await page.evaluateHandle(() => new DataTransfer());
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await header.dispatchEvent("dragstart", { dataTransfer });
  // The grouping plugin marks the header a tick after dragstart; a drop
  // before that is ignored.
  await expect(header).toHaveClass(/slick-header-column-active/);
  for (const type of ["dragenter", "dragover", "drop"]) {
    await page.evaluate(
      ([type, x, y, dataTransfer]) => {
        const target = document.elementFromPoint(x, y);
        if (!target) throw new Error(`nothing at (${x}, ${y}) to drop on`);
        target.dispatchEvent(
          new DragEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, dataTransfer }),
        );
      },
      [type, to.x + 40, to.y + to.height / 2, dataTransfer] as const,
    );
  }
  await page.mouse.up();
}

test("a header dragged into the bar groups the lines by that column", async ({ page }) => {
  const dialog = await openServerLog(page);
  const header = dialog.locator('.slick-header-column[col-id="level"]');
  const bar = dialog.locator(".slick-preheader-panel .slick-dropzone");
  await expect(bar).toContainText("Drag a column here");

  await dragHeaderInto(page, header, bar);

  const group = dialog.locator(".rl-grid .slick-row.slick-group").first();
  await expect(group).toBeVisible();
  await expect(group).toHaveText(/^Level: \w+ \(\d+\)$/);
  await expect(dialog.locator(".slick-group-toggle-all")).toContainText("Expand / collapse all");
});
