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
// We assert via the row count alone — qmd's hybrid ranking is not pinned
// to specific UUIDs, so we don't check identities.

test.describe("free-text search routes through qmd", () => {
  test("bare 'grey earl' returns rows (qmd hybrid; was zero under LIKE)", async ({
    page,
  }) => {
    await page.goto("/#/search?q=grey%20earl");
    const firstDataRow = page
      .locator('.ag-center-cols-container [role="row"]')
      .first();
    // qmd's first run can take a few seconds (npx warm-up + model load).
    await expect(firstDataRow).toBeVisible({ timeout: 30_000 });
  });

  test("explicit qmd:\"...\" predicate also returns rows", async ({ page }) => {
    await page.goto('/#/search?q=qmd%3A%22earl%20grey%22');
    const firstDataRow = page
      .locator('.ag-center-cols-container [role="row"]')
      .first();
    await expect(firstDataRow).toBeVisible({ timeout: 30_000 });
  });

  test("qmd_vsearch:\"...\" predicate routes to vector-only mode", async ({
    page,
  }) => {
    await page.goto('/#/search?q=qmd_vsearch%3A%22earl%20grey%22');
    const firstDataRow = page
      .locator('.ag-center-cols-container [role="row"]')
      .first();
    await expect(firstDataRow).toBeVisible({ timeout: 30_000 });
  });
});
