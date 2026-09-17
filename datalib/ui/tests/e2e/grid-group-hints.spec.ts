// The row-group bar says what it is for, before and after it is used.
//
// Grouping this grid by source is the single most useful thing to do
// with it, and the bar that does it reads as decoration until you know
// that. The placeholder carries the explanation while nothing is
// grouped; once something is, its place is taken by a chip for the
// grouped column, with the grouping's sort and a way to drop it.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, type GridApi } from "./grid-helpers";

/// Our placeholder, in the grid's own drop zone.
const placeholder = (page: Page) =>
  page.locator(".grid-box .slick-draggable-dropzone-placeholder");

async function openGrid(page: Page) {
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
}

/// Group by a column the way the bar's own drop does.
async function groupBy(page: Page, colId: string) {
  await page.evaluate(
    (id) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.groupBy([id]),
    colId,
  );
}

test("the empty row-group bar explains what dropping a column there does", async ({
  page,
}) => {
  await openGrid(page);

  // Nothing is grouped by default, so this is the state a new user
  // meets. The grid's stock text names the mechanism ("drop a column
  // header here to group by"); ours names the result.
  await expect(page.locator(".grid-box .slick-group")).toHaveCount(0);
  await expect(placeholder(page)).toContainText("group rows by them");
});

test("once grouped, the bar holds a chip for the column and the rows fold under it", async ({
  page,
}) => {
  await openGrid(page);
  await groupBy(page, "source_ref");

  // The bar now holds a chip for the grouped column instead of the
  // placeholder…
  const chip = page.locator(".grid-box .slick-dropped-grouping");
  await expect(chip).toContainText("Source");
  await expect(placeholder(page)).toBeHidden();

  // …and the rows sit under group rows that say what they share and
  // how many there are. From the top: the grid lands at its end, where
  // the group row the last rows sit under is above the viewport.
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.scrollToRow(0),
  );
  const group = page.locator(".grid-box .slick-group").first();
  await expect(group).toBeVisible();
  await expect(group).toContainText(/Source: .+ \(\d+\)/);
});
