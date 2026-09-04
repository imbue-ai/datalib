import { test, expect } from "@playwright/test";
import { searchAndSettle } from "./grid-helpers";

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
  // The `warmup` project pays the qmd cold start before any spec runs,
  // but the applet restarts on config changes and `manager2-sync`
  // (which sorts earlier) rewrites config.toml — so this may still land
  // on a freshly respawned daemon and pay the model load again. See
  // `SEARCH_SETTLE`.
  test.setTimeout(180_000);

  test("score column is non-increasing and viewport lands at row 0", async ({
    page,
  }) => {
    // 1. Open the search page empty. Time-asc default scrolls to the
    //    bottom, so we have a non-zero scrollTop — the precondition
    //    for the scroll-to-top assertion below.
    await page.goto("/");
    await page
      .locator('.ag-grid-scrolling-rows [role="row"]')
      .first()
      .waitFor({ timeout: 10_000 });
    const viewport = page.locator(".ag-grid-viewport");
    await expect(viewport).toBeVisible();
    const beforeScrollTop = await viewport.evaluate((el) => el.scrollTop);
    expect(
      beforeScrollTop,
      "fixture must have enough rows that the time-asc default scrolls past the top",
    ).toBeGreaterThan(0);

    // 2. Type a free-text query — qmd routes it.
    //
    // `searchAndSettle` absorbs the search latency, so the two
    // assertions below are about the *result*, not about waiting: the
    // score column exists only on qmd-routed rows, so once the grid is
    // painting this query, its presence is the proof that the query
    // routed. Gating on the header with a 90s timeout used to conflate
    // those two things — a slow daemon and a query that never reached
    // qmd failed identically.
    await searchAndSettle(page, "grey earl");

    const scoreHeader = page.locator('.ag-header-cell[col-id="score"]');
    await expect(scoreHeader).toBeVisible();
    const firstRow = page
      .locator('.ag-grid-scrolling-rows [role="row"]')
      .first();
    await expect(firstRow).toBeVisible();

    // 3. Viewport must have scrolled to row 0. AG Grid's
    //    `ensureIndexVisible(0, "top")` writes scrollTop near 0 (browser
    //    may add a sub-pixel for alignment). Poll briefly to absorb
    //    the post-sort layout settle.
    //
    //    Asserted BEFORE the score column, and the order is the point.
    //    This scroll is the last thing `applyDefaultSort` does — it
    //    hooks `rowDataUpdated` and a double rAF — so a viewport that
    //    has landed at the top is the signal that the sort pipeline has
    //    finished and the rendered window is final. Reading the column
    //    first meant reading it *through* that re-render.
    await expect
      .poll(async () => viewport.evaluate((el) => el.scrollTop), {
        timeout: 5_000,
        message: "qmd result viewport should land at the top",
      })
      .toBeLessThan(5);

    // 4. Score column values are non-increasing in DOM order.
    //    Virtualization means we only see the on-screen window, but a
    //    non-increasing prefix is enough to assert the sort direction.
    //
    //    Read in ONE page evaluation rather than `cells.nth(i)` in a
    //    loop. Eighteen round trips take long enough for the grid to
    //    re-render between them, and a walk that spans a re-render
    //    splices cells from two different renders — which is how a
    //    descending column read `0.13, 0.14` in CI (webkit, run
    //    33871259668). One evaluation is one DOM state, so the sequence
    //    it returns is a sequence that actually existed.
    const cells = page.locator(
      '.ag-grid-scrolling-rows [role="row"] [col-id="score"]',
    );
    const texts = await cells.evaluateAll((els) =>
      els.map((el) => (el.textContent ?? "").trim()),
    );
    expect(texts.length, "expected qmd-routed search to surface score cells")
      .toBeGreaterThan(1);

    const values: number[] = [];
    texts.forEach((txt, i) => {
      // Skip rows with no score (shouldn't happen on a qmd query, but
      // be defensive about empty cells while data streams in).
      if (txt.length === 0) return;
      const n = Number(txt);
      expect(
        Number.isFinite(n),
        `score cell ${i} not a number: ${JSON.stringify(txt)}`,
      ).toBeTruthy();
      values.push(n);
    });

    expect(values.length, "no numeric score cells visible").toBeGreaterThan(1);
    for (let i = 1; i < values.length; i++) {
      expect(
        values[i] <= values[i - 1],
        `scores not non-increasing at index ${i}: ${values.join(", ")}`,
      ).toBeTruthy();
    }
  });
});
