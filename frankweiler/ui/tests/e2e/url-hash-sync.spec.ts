import { test, expect } from "@playwright/test";

// Bug #1: the URL should encode app state — at minimum the selected
// row, plus column visibility / widths / ordering — so the URL is a
// reload-stable deeplink to the user's current view.
//
// The app uses createWebHistory() with a path-encoded column stack
// (see `src/router/columns.ts`): `/grid:q=…&sel=…&ag=…/doc:<uuid>`,
// each path segment being one Miller column.
//
// This is a black-box contract: the test asserts the URL actually
// changes in response to user actions. It does not pin a
// serialization format — the implementer is free to choose one.

// Resolve a stable target row by its `row-id` (AG Grid's per-row UUID
// attribute). `.first()` in a virtualized grid is racy: after a sort or
// scroll, the row at DOM-position-0 can shift mid-test, so a click and
// the subsequent class-assertion may end up looking at different rows.
async function pinFirstRowId(page: import("@playwright/test").Page) {
  const first = page.locator('.ag-center-cols-container [role="row"]').first();
  await expect(first).toBeVisible();
  const id = await first.getAttribute("row-id");
  expect(id, "first data row must have a row-id attribute").toBeTruthy();
  return id!;
}

test.describe("URL reflects app state (bug #1)", () => {
  test("selecting a row updates the URL", async ({ page }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);
    const target = page.locator(
      `.ag-center-cols-container [role="row"][row-id="${rowId}"]`,
    );

    const beforePath = await page.evaluate(() => location.pathname);

    await target.click();
    // Selection visibly applies (row gets ag-row-selected class).
    await expect(target).toHaveClass(/ag-row-selected/);

    const afterPath = await page.evaluate(() => location.pathname);
    expect(
      afterPath,
      `expected path to change after row selection (was ${beforePath})`,
    ).not.toBe(beforePath);
  });

  test("URL survives reload — selected row is restored", async ({ page }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);
    const target = page.locator(
      `.ag-center-cols-container [role="row"][row-id="${rowId}"]`,
    );
    await target.click();
    await expect(target).toHaveClass(/ag-row-selected/);

    const pathWithSelection = await page.evaluate(() => location.pathname);
    // A bare grid column with no state would just be `/grid`; selecting
    // a row should add inline params (`/grid:…sel=…`) or push a doc
    // column. Either way the path is no longer the default.
    expect(pathWithSelection).not.toBe("/");
    expect(pathWithSelection).not.toBe("/grid");

    // Reload at the same URL — the selection should come back.
    await page.reload();
    const restoredRow = page
      .locator('.ag-center-cols-container [role="row"].ag-row-selected')
      .first();
    await expect(restoredRow).toBeVisible({ timeout: 10_000 });
  });
});
