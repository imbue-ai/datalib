import { test, expect } from "@playwright/test";

// Bug #1: the URL hash should encode app state — at minimum the selected
// row, plus column visibility / widths / ordering — so the URL is a
// reload-stable deeplink to the user's current view.
//
// The app uses createWebHashHistory(), so the route itself lives in the
// hash (#/search). State should appear *after* the route as a query
// string (e.g. `#/search?selected=<conv-uuid>&cols=...`).
//
// This is a black-box contract: the test just asserts the hash actually
// changes in response to user actions. It does not pin a serialization
// format — the implementer is free to choose one.

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

test.describe("URL hash reflects app state (bug #1)", () => {
  test("selecting a row updates the URL hash", async ({ page }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);
    const target = page.locator(
      `.ag-center-cols-container [role="row"][row-id="${rowId}"]`,
    );

    const beforeHash = await page.evaluate(() => location.hash);

    await target.click();
    // Selection visibly applies (row gets ag-row-selected class).
    await expect(target).toHaveClass(/ag-row-selected/);

    const afterHash = await page.evaluate(() => location.hash);
    expect(
      afterHash,
      `expected hash to change after row selection (was ${beforeHash})`,
    ).not.toBe(beforeHash);
  });

  test("hash survives reload — selected row is restored", async ({ page }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);
    const target = page.locator(
      `.ag-center-cols-container [role="row"][row-id="${rowId}"]`,
    );
    await target.click();
    await expect(target).toHaveClass(/ag-row-selected/);

    const hashWithSelection = await page.evaluate(() => location.hash);
    expect(hashWithSelection).not.toBe("#/search");

    // Reload at the same URL — the selection should come back.
    await page.reload();
    const restoredRow = page
      .locator('.ag-center-cols-container [role="row"].ag-row-selected')
      .first();
    await expect(restoredRow).toBeVisible({ timeout: 10_000 });
  });
});
