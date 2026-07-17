import { test, expect } from "@playwright/test";

// The URL path encodes the whole column stack: a /-separated list of
// `code:state` segments (see src/router/columns.ts), where `code` is
// the card source (e.g. `gridView()`) and `state` is the card's
// opaque persisted state. Selecting a grid row both opens a
// `documentView(…)` column and lands the selection in the grid's
// state — so the URL is a reload-stable deeplink to the user's
// current view.
//
// This is a black-box contract: the test asserts the URL changes in
// response to user actions and that a reload restores the view. It
// does not pin the state-string serialization.

// Resolve a stable target row by its `row-id` (AG Grid's per-row UUID
// attribute). `.first()` in a virtualized grid is racy: after a sort or
// scroll, the row at DOM-position-0 can shift mid-test, so a click and
// the subsequent class-assertion may end up looking at different rows.
async function pinFirstRowId(page: import("@playwright/test").Page) {
  const first = page.locator('.ag-center-cols-container [role="row"]').first();
  await expect(first).toBeVisible({ timeout: 10_000 });
  const id = await first.getAttribute("row-id");
  expect(id, "first data row must have a row-id attribute").toBeTruthy();
  return id!;
}

test.describe("URL reflects app state", () => {
  test("selecting a row updates the URL and opens a document column", async ({
    page,
  }) => {
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
    // The row's document opened as a second column, and the column
    // stack (not just query params) carries it.
    expect(decodeURIComponent(afterPath)).toContain("documentView(");
    await expect(page.locator(".chat-preview")).toBeVisible();
  });

  test("URL survives reload — selection and document column restored", async ({
    page,
  }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);
    const target = page.locator(
      `.ag-center-cols-container [role="row"][row-id="${rowId}"]`,
    );
    await target.click();
    await expect(target).toHaveClass(/ag-row-selected/);
    await expect(page.locator(".chat-preview")).toBeVisible();

    const pathWithSelection = await page.evaluate(() => location.pathname);
    expect(pathWithSelection).not.toBe("/");

    // Reload at the same URL — both the grid selection and the
    // document column should come back, without the restore opening
    // a duplicate document column.
    await page.reload();
    const restoredRow = page
      .locator('.ag-center-cols-container [role="row"].ag-row-selected')
      .first();
    await expect(restoredRow).toBeVisible({ timeout: 10_000 });
    await expect(restoredRow).toHaveAttribute("row-id", rowId);
    await expect(page.locator(".chat-preview")).toHaveCount(1, {
      timeout: 10_000,
    });
  });

  test("editing a column's source via the header re-runs the card", async ({
    page,
  }) => {
    // The editable source box only exists in dev mode; default chrome
    // shows titles.
    await page.addInitScript(() => localStorage.setItem("fw-dev-mode", "1"));
    await page.goto("/");
    await pinFirstRowId(page);
    // "+" appends a blank column in dev mode; its source box accepts
    // new card source, and committing it materializes the card.
    await page.locator(".miller-add").click();
    const boxes = page.locator(".miller-col-source");
    await expect(boxes).toHaveCount(2); // grid + new blank
    const blank = boxes.last();
    await blank.fill("documentView()");
    await blank.press("Enter");
    await expect(page.locator(".chat-preview")).toBeVisible({
      timeout: 10_000,
    });
    expect(
      decodeURIComponent(await page.evaluate(() => location.pathname)),
    ).toContain("documentView()");
  });
});
