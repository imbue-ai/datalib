import { test, expect } from "@playwright/test";

import { firstRowUuid, selectRowByUuid, type GridApi } from "./grid-helpers";

// The URL path encodes the whole column stack: a /-separated list of
// `code:state` segments (see src/router/columns.ts), where `code` is
// the card source (e.g. `gridView()`) and `state` is the card's
// opaque persisted state. Selecting a grid row both opens a
// `documentView(…)` column and lands the selection in the grid's
// state — so the URL is a reload-stable deeplink to the user's
// current view.

// Resolve a stable target row by its uuid rather than `.first()`: in a
// virtualized grid the row at DOM-position-0 can shift mid-test after a
// sort or scroll, so a click and the subsequent assertion may end up
// looking at different rows.
//
// The id is then handed to `selectRowByUuid`, which scrolls that row
// into view before clicking it and confirms the selection took. Both
// halves matter here: the row this pins can be outside the viewport
// (a fresh load does not always start at the top of the collection),
// and the click races the app's own view restore.
async function pinFirstRowId(page: import("@playwright/test").Page) {
  return firstRowUuid(page);
}

test.describe("URL reflects app state", () => {
  test("selecting a row updates the URL and opens a document column", async ({
    page,
  }) => {
    await page.goto("/");
    const rowId = await pinFirstRowId(page);

    const beforePath = await page.evaluate(() => location.pathname);

    // Selection visibly applies (row gets ag-row-selected class).
    await selectRowByUuid(page, rowId);

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
    await selectRowByUuid(page, rowId);
    await expect(page.locator(".chat-preview")).toBeVisible();

    const pathWithSelection = await page.evaluate(() => location.pathname);
    expect(pathWithSelection).not.toBe("/");

    // Reload at the same URL — both the grid selection and the
    // document column should come back, without the restore opening
    // a duplicate document column.
    await page.reload();
    await expect(page.locator(".grid-box .slick-cell.selected").first()).toBeVisible({
      timeout: 10_000,
    });
    // The selection the grid holds is the row pinned above, not merely
    // some row.
    await expect
      .poll(() =>
        page.evaluate(
          (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
          rowId,
        ),
      )
      .toBe(true);
    await expect(page.locator(".chat-preview")).toHaveCount(1, {
      timeout: 10_000,
    });
  });

  test("editing a column's source via the header re-runs the card", async ({
    page,
  }) => {
    // The editable source box only exists in dev mode; default chrome
    // shows titles.
    await page.addInitScript(() => localStorage.setItem("datalib-dev-mode", "1"));
    await page.goto("/");
    await pinFirstRowId(page);
    // "+" appends a gallery column (both modes); in dev mode its
    // source box is editable — overwrite it with new card source and
    // commit to materialize the card.
    await page.locator(".miller-add").click();
    const boxes = page.locator(".miller-col-source");
    await expect(boxes).toHaveCount(2); // grid + new gallery
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
