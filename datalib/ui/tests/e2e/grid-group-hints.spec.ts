// The row-group bar says what it is for, before and after it is used.
//
// Grouping this grid by source is the single most useful thing to do
// with it, and the bar that does it reads as decoration until you know
// that. Two pieces of text carry the explanation, and they are mutually
// exclusive — the placeholder is only there while nothing is grouped,
// and the group column only exists once something is — so both need
// pinning or half the help can vanish unnoticed.

import { test, expect, type Page } from "@playwright/test";

const ROWS = '.ag-grid-scrolling-rows [role="row"]';

/// Our placeholder, wherever AG Grid renders it. There is more than one
/// empty row-group drop zone on screen (the bar above the grid, and the
/// columns tool panel's own), and both carry this text — which is the
/// point, so this asserts on the text rather than trying to name one of
/// them through the widget's internal classes.
const placeholder = (page: Page) =>
  page.locator(".ag-column-drop-empty-message").first();

async function openGrid(page: Page) {
  await page.goto("/");
  await page.locator(ROWS).first().waitFor({ timeout: 15_000 });
}

/// Group by a column the way the panel's own drag does.
async function groupBy(page: Page, colId: string) {
  await page.evaluate((id) => {
    const w = window as unknown as {
      __fwGridApi?: {
        applyColumnState: (p: {
          state: { colId: string; rowGroup: boolean }[];
        }) => void;
      };
    };
    w.__fwGridApi!.applyColumnState({ state: [{ colId: id, rowGroup: true }] });
  }, colId);
}

test("the empty row-group bar explains what dropping a column there does", async ({
  page,
}) => {
  await openGrid(page);

  // Nothing is grouped by default, so this is the state a new user
  // meets. AG Grid's stock text names the mechanism ("set row groups");
  // ours names the result.
  await expect(page.locator(ROWS + '[row-id^="row-group-"]')).toHaveCount(0);
  await expect(placeholder(page)).toContainText("group rows by them");
});

test("once grouped, the group column explains how to change it", async ({
  page,
}) => {
  await openGrid(page);
  await groupBy(page, "source_name");

  const header = page.locator('.ag-header-cell[col-id="ag-Grid-AutoColumn"]');
  await expect(header).toBeVisible();

  // Hover-revealed rather than a native `title`: AG Grid renders its own
  // tooltip unless `enableBrowserTooltips` is set, and the other
  // `headerTooltip`s in this grid already rely on that.
  await header.hover();
  await expect(page.locator(".ag-tooltip")).toContainText(
    "Drag a column into the bar above",
  );

  // The bar itself now holds a chip for the grouped column instead of
  // the placeholder — which is the whole reason the second half of the
  // help had to live on the group column.
  await expect(page.getByRole("listbox", { name: "Row Groups" })).toContainText(
    "Source",
  );
});
