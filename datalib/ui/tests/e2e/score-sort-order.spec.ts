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
      .locator(".grid-box .slick-row")
      .first()
      .waitFor({ timeout: 10_000 });
    // The main pane's viewport: the grid keeps one per frozen quadrant.
    const viewport = page.locator(".grid-box .slick-viewport-top.slick-viewport-left");
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

    const scoreHeader = page.locator('.grid-box .slick-header-column[col-id="score"]');
    await expect(scoreHeader).toBeVisible();
    // Its filter's operator dropdown shows an operator or nothing —
    // never the `&nbsp;` padding slickgrid writes as text when HTML
    // rendering is off (typedColumns.FILTER_GRID_OPTIONS).
    const operators = page.locator(".grid-box .slick-headerrow .filter-score select");
    await expect(operators).toHaveCount(1);
    expect(await operators.innerText()).not.toContain("&nbsp;");
    const firstRow = page
      .locator(".grid-box .slick-row")
      .first();
    await expect(firstRow).toBeVisible();

    // 3. Viewport must have scrolled to row 0. `scrollRowIntoView(0)`
    //    writes scrollTop near 0 (browser may add a sub-pixel for
    //    alignment). Poll briefly to absorb the post-sort layout settle.
    await expect
      .poll(async () => viewport.evaluate((el) => el.scrollTop), {
        timeout: 5_000,
        message: "qmd result viewport should land at the top",
      })
      .toBeLessThan(5);

    // 4. Score column values are non-increasing in row order.
    //    Virtualization means we only see the on-screen window, but a
    //    non-increasing prefix is enough to assert the sort direction.
    //    Row order, not DOM order: the grid appends a row's node when
    //    it first scrolls in, so the DOM is not sorted.
    const cells = page.locator(
      '.grid-box .slick-row [col-id="score"]',
    );
    const texts = await cells.evaluateAll((els) =>
      els
        .map((el) => ({
          row: Number(el.closest(".slick-row")?.getAttribute("data-row") ?? -1),
          text: (el.textContent ?? "").trim(),
        }))
        .sort((a, b) => a.row - b.row)
        .map((c) => c.text),
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
