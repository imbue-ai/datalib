import { test, expect } from "@playwright/test";

// Contract: when a search routes through qmd, the result grid must be
// sorted by the Score column descending. Earlier we shipped a Time-desc
// default sort that hid the qmd rank — e.g. searching for the photography
// note returned chat-glenn rows at the top because they were newer, even
// though qmd's top hit was an older note. The fix added a Score column
// with `sort: "desc", sortIndex: 0`; this test pins that contract by
// reading the visible Score cells in DOM order and asserting non-increasing.

test.describe("qmd search sorts by score desc", () => {
  // First qmd call in a session warms up the npx package + the model;
  // give it a generous budget.
  test.setTimeout(120_000);

  test("score column is present and rows are sorted by it descending", async ({
    page,
  }) => {
    await page.goto("/#/search?q=grey%20earl");

    // Score column header exists.
    const scoreHeader = page.locator(
      '.ag-header-cell[col-id="score"]',
    );
    await expect(scoreHeader).toBeVisible({ timeout: 30_000 });

    // Wait for results.
    const firstRow = page
      .locator('.ag-center-cols-container [role="row"]')
      .first();
    await expect(firstRow).toBeVisible({ timeout: 90_000 });

    // Read the score cells in render order. Virtualization means we only
    // see the on-screen window, but a non-increasing prefix is enough to
    // assert the sort direction.
    const cells = page.locator(
      '.ag-center-cols-container [role="row"] [col-id="score"]',
    );
    const count = await cells.count();
    expect(count, "expected qmd-routed search to surface score cells")
      .toBeGreaterThan(1);

    const values: number[] = [];
    for (let i = 0; i < count; i++) {
      const txt = (await cells.nth(i).innerText()).trim();
      // Skip rows with no score (shouldn't happen on a qmd query, but be
      // defensive about empty cells while data streams in).
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
  });
});
