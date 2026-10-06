import { test, expect } from "@playwright/test";
import { GRID, firstRowUuid, selectRowByUuid } from "./grid-helpers";

// A column a card opens (a grid row's document) is scrolled into view.
// A Columns container scrolls sideways, and one wider than the window
// used to open its new column off the right edge with nothing moving to
// show it — the click looked like it did nothing.

// Narrow enough that the default grid column (640px) and the document
// column it opens (640px) cannot both fit.
test.use({ viewport: { width: 900, height: 700 } });

// How far each edge of the document column sits outside its Columns
// container, in px: 0 when the edge is inside.
async function docColumnOverhang(page: import("@playwright/test").Page) {
  return page.locator(".ct-child", { has: page.locator(".chat-preview") }).evaluate((el) => {
    const row = el.closest(".ct-children--columns")!.getBoundingClientRect();
    const col = el.getBoundingClientRect();
    return { left: Math.max(0, row.left - col.left), right: Math.max(0, col.right - row.right) };
  });
}

test("opening a document scrolls the new column into view", async ({ page }) => {
  await page.goto(GRID);
  const rowId = await firstRowUuid(page);
  await selectRowByUuid(page, rowId);
  await expect(page.locator(".chat-preview")).toBeVisible();

  // Before the reveal lands the column overhangs the row by hundreds of
  // px; the reveal is a smooth scroll, so poll for where it settles.
  // ±1 for sub-pixel layout.
  await expect
    .poll(
      async () => {
        const { left, right } = await docColumnOverhang(page);
        return left <= 1 && right <= 1;
      },
      { message: "the document column must end up entirely inside its container" },
    )
    .toBe(true);
});
