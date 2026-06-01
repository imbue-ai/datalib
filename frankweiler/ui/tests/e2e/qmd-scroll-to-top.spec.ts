import { test, expect } from "@playwright/test";

// Contract: when a search routes through qmd (adds a Score column,
// sorts desc by score), the result grid scrolls to the top so the
// highest-ranked hits are immediately visible. Without this, the
// viewport keeps whatever scroll offset the previous time-asc-at-
// bottom default left it on, and the user sees row N+1 instead of
// row 0 with no obvious way to know the sort changed.

test.describe("qmd query scrolls grid to top", () => {
  // The first qmd call warms up npx + the embedding model; give it
  // generous budget.
  test.setTimeout(120_000);

  test("after a qmd query, viewport is at row 0", async ({ page }) => {
    // 1. Open the search page with an empty query — time-asc default
    //    scrolls to the bottom. This is the bug's starting state:
    //    the grid is scrolled past row 0 and the user is about to
    //    issue a qmd query that should land them back at the top.
    await page.goto("/");
    await page
      .locator('.ag-center-cols-container [role="row"]')
      .first()
      .waitFor({ timeout: 10_000 });
    const viewport = page.locator(".ag-body-viewport");
    await expect(viewport).toBeVisible();

    // Sanity check: the default time-asc-at-bottom scroll has moved
    // us off row 0. (If the fixture is too small for vertical scroll
    // this assertion goes vacuous; the TNG fixture is large enough.)
    const beforeScrollTop = await viewport.evaluate((el) => el.scrollTop);
    expect(
      beforeScrollTop,
      "fixture must have enough rows that the default scrolls past the top",
    ).toBeGreaterThan(0);

    // 2. Type a free-text query into the search box. The 150ms
    //    debounce + qmd round-trip kicks off a qmd-routed search;
    //    the Score column appears when results land.
    await page
      .getByTestId("search-input")
      .fill("grey earl");

    const scoreHeader = page.locator('.ag-header-cell[col-id="score"]');
    await expect(scoreHeader).toBeVisible({ timeout: 90_000 });
    await page
      .locator('.ag-center-cols-container [role="row"]')
      .first()
      .waitFor({ timeout: 30_000 });

    // 3. Viewport should be at the top after the qmd result landed —
    //    the user is looking at the highest-ranked hits, not whatever
    //    offset the previous time-asc default left them at.
    //    `ensureIndexVisible(0, "top")` writes scrollTop near 0
    //    (browser may add a sub-pixel for alignment).
    await expect
      .poll(
        async () => viewport.evaluate((el) => el.scrollTop),
        {
          timeout: 5_000,
          message: "qmd result viewport should land at the top",
        },
      )
      .toBeLessThan(5);
  });
});
