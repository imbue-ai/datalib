// The grid searches as you type, so it is often handed a filter not yet
// finished: `is:do` on the way to `is:document`. It used to toast the
// refusal and empty the grid mid-word. A query the search cannot read
// keeps the rows it had, marked as from the previous search, and says
// why quietly beside the bar.
import { test, expect } from "@playwright/test";
import { EVERY_ROW, SEARCH_ROWS, typeInto } from "./grid-helpers";

test("a filter still being typed keeps the rows and raises no toast", async ({ page }) => {
  await page.goto(EVERY_ROW);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 10_000 });
  const input = page.getByTestId("search-input");

  await typeInto(input, "is:do");
  const hint = page.getByRole("status").filter({ hasText: "`is:do`" });
  await expect(hint).toBeVisible();
  await expect(page.locator(".grid--stale")).toHaveCount(1);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible();
  await expect(page.locator(".datalib-toast")).toHaveCount(0);

  await typeInto(input, "is:document");
  await expect(hint).toHaveCount(0);
  await expect(page.locator(".grid--stale")).toHaveCount(0);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible();
  await expect(page.locator(".datalib-toast")).toHaveCount(0);
});
