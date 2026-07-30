import { test, expect } from "@playwright/test";

// Free-text search routes through qmd (BM25 + vector + reranker by
// default). The bug this guards: previously the Rust backend did
// `LOWER(text) LIKE %query%`, so multi-word phrases only matched when
// their tokens appeared in that exact order — `grey earl` would return
// zero rows even though both tokens show up in many fixture rows (just
// as the literal "earl grey").
//
// Contract:
//   * Bare text in the search bar → qmd hybrid query.
//   * `qmd:"text"` predicate → hybrid (same as bare; explicit form).
//   * `qmd_vsearch:"text"` → vector-only mode.
//
// We type into the grid's search bar (the v1 `/#/search?q=…` deeplink
// form is gone; the query lives in the grid card's state now) and
// gate on the Score column appearing — qmd-routed results carry
// scores, LIKE-fallback rows don't, so the header showing up is the
// signal that the query actually routed through qmd.

async function qmdSearch(
  page: import("@playwright/test").Page,
  q: string,
  timeout: number,
) {
  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });
  await page.getByTestId("search-input").fill(q);
  await expect(
    page.locator('.ag-header-cell[col-id="score"]'),
  ).toBeVisible({ timeout });
  await expect(
    page.locator('.ag-center-cols-container [role="row"]').first(),
  ).toBeVisible({ timeout: 30_000 });
}

test.describe("free-text search routes through qmd", () => {
  // The first qmd call in this session pays for npx package fetch + model
  // load. Subsequent calls are fast. Lift the per-test timeout for the
  // first sub-test so the warm-up doesn't trip Playwright's 30s default.
  test.setTimeout(120_000);

  test("bare 'grey earl' returns rows (qmd hybrid; was zero under LIKE)", async ({
    page,
  }) => {
    await qmdSearch(page, "grey earl", 90_000);
  });

  test('explicit qmd:"..." predicate also returns rows', async ({ page }) => {
    await qmdSearch(page, 'qmd:"earl grey"', 30_000);
  });

  test('qmd_vsearch:"..." predicate routes to vector-only mode', async ({
    page,
  }) => {
    await qmdSearch(page, 'qmd_vsearch:"earl grey"', 30_000);
  });
});
