import { test, expect } from "@playwright/test";

// Two contracts a single qmd-routed query has to satisfy. They share
// the same setup (open page → type free-text → qmd routes → score
// column appears), so they ride one test to avoid paying for qmd
// warm-up twice.
//
// 1. **Score sort**: when a search routes through qmd the grid must
//    be sorted by the Score column descending. We used to ship a
//    Time-desc default that hid the qmd rank — searching for the
//    photography note returned chat-glenn rows at the top because
//    they were newer, even though qmd's top hit was an older note.
//    The fix added a Score column with `sort: "desc", sortIndex: 0`;
//    this assertion reads the visible Score cells in DOM order and
//    asserts non-increasing.
//
// 2. **Scroll to top**: the default empty-query sort is time-asc and
//    `applyDefaultSort` scrolls the viewport to the *bottom* so the
//    user lands on the most recent rows. Issuing a qmd query has to
//    flip the viewport back to row 0 so the highest-ranked hits are
//    immediately visible — otherwise the user sees row N+1 of the
//    qmd-sorted set with no signal that the sort changed.
//    `applyDefaultSort` hooks AG Grid's `rowDataUpdated` event +
//    a double-rAF fallback to land the scroll write after the
//    virtualizer ingests the new rowData.

test.describe("qmd-routed search: score-desc sort + scroll-to-top", () => {
  // First qmd call in a session warms up the npx package + the model;
  // give it a generous budget.
  test.setTimeout(120_000);

  test("score column is non-increasing and viewport lands at row 0", async ({
    page,
  }) => {
    // 1. Open the search page empty. Time-asc default scrolls to the
    //    bottom, so we have a non-zero scrollTop — the precondition
    //    for the scroll-to-top assertion below.
    await page.goto("/");
    await page
      .locator('.ag-center-cols-container [role="row"]')
      .first()
      .waitFor({ timeout: 10_000 });
    const viewport = page.locator(".ag-body-viewport");
    await expect(viewport).toBeVisible();
    const beforeScrollTop = await viewport.evaluate((el) => el.scrollTop);
    expect(
      beforeScrollTop,
      "fixture must have enough rows that the time-asc default scrolls past the top",
    ).toBeGreaterThan(0);

    // 2. Type a free-text query — qmd routes it.
    await page.getByTestId("search-input").fill("grey earl");

    // Score column appears when qmd returns and the result set is in
    // place.
    const scoreHeader = page.locator('.ag-header-cell[col-id="score"]');
    await expect(scoreHeader).toBeVisible({ timeout: 90_000 });
    const firstRow = page
      .locator('.ag-center-cols-container [role="row"]')
      .first();
    await expect(firstRow).toBeVisible({ timeout: 30_000 });

    // 3. Score column values are non-increasing in DOM order.
    //    Virtualization means we only see the on-screen window, but a
    //    non-increasing prefix is enough to assert the sort direction.
    const cells = page.locator(
      '.ag-center-cols-container [role="row"] [col-id="score"]',
    );
    const count = await cells.count();
    expect(count, "expected qmd-routed search to surface score cells")
      .toBeGreaterThan(1);

    const values: number[] = [];
    for (let i = 0; i < count; i++) {
      const txt = (await cells.nth(i).innerText()).trim();
      // Skip rows with no score (shouldn't happen on a qmd query, but
      // be defensive about empty cells while data streams in).
      if (txt.length === 0) continue;
      const n = Number(txt);
      expect(
        Number.isFinite(n),
        `score cell ${i} not a number: ${JSON.stringify(txt)}`,
      ).toBeTruthy();
      values.push(n);
    }

    expect(values.length, "no numeric score cells visible").toBeGreaterThan(1);
    for (let i = 1; i < values.length; i++) {
      expect(
        values[i] <= values[i - 1],
        `scores not non-increasing at index ${i}: ${values.join(", ")}`,
      ).toBeTruthy();
    }

    // 4. Viewport must have scrolled to row 0. AG Grid's
    //    `ensureIndexVisible(0, "top")` writes scrollTop near 0 (browser
    //    may add a sub-pixel for alignment). Poll briefly to absorb
    //    the post-sort layout settle.
    await expect
      .poll(async () => viewport.evaluate((el) => el.scrollTop), {
        timeout: 5_000,
        message: "qmd result viewport should land at the top",
      })
      .toBeLessThan(5);
  });
});
