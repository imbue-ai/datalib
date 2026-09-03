import { test, expect } from "@playwright/test";
import { searchAndSettle } from "./grid-helpers";

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

async function qmdSearch(page: import("@playwright/test").Page, q: string) {
  await page.goto("/");
  await page
    .locator('.ag-grid-scrolling-rows [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });
  // Settle first, then assert. The score column is only ever populated
  // by qmd-routed rows, so once the grid is painting this query its
  // presence *is* the routing assertion — no timeout needed, and a
  // query that silently stopped routing now fails immediately instead
  // of after a 90s wait indistinguishable from a slow daemon.
  await searchAndSettle(page, q);
  await expect(page.locator('.ag-header-cell[col-id="score"]')).toBeVisible();
  await expect(
    page.locator('.ag-grid-scrolling-rows [role="row"]').first(),
  ).toBeVisible();
}

test.describe("free-text search routes through qmd", () => {
  // The `warmup` project pays the cold start before any spec runs, but
  // the applet restarts on config changes and `manager2-sync` (which
  // sorts earlier) rewrites config.toml wholesale — so the first query
  // here may still pay a qmd model load. See `SEARCH_SETTLE`.
  test.setTimeout(180_000);

  test("bare 'grey earl' returns rows (qmd hybrid; was zero under LIKE)", async ({
    page,
  }) => {
    await qmdSearch(page, "grey earl");
  });

  test('explicit qmd:"..." predicate also returns rows', async ({ page }) => {
    await qmdSearch(page, 'qmd:"earl grey"');
  });

  test('qmd_vsearch:"..." predicate routes to vector-only mode', async ({
    page,
  }) => {
    await qmdSearch(page, 'qmd_vsearch:"earl grey"');
  });
});
