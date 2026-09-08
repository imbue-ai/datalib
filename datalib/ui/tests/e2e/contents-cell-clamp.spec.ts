import { test, expect } from "@playwright/test";
import { actOnRowByUuid, expectGridPainted } from "./grid-helpers";

// What this test pins:
//   The Contents column in the search grid must render long snippets at
//   *exactly two lines* of text with an ellipsis cutting off the rest.
//   The row height is fixed (autoHeight is intentionally off — per-row
//   measurement was the dominant render cost on large result sets), so
//   getting the clamp wrong is visually ugly: either a stray partial
//   third line leaks out the bottom of the row, or the snippet
//   collapses to a single line and wastes half the row's vertical space.
//
// How it checks:
//   1. Find a fixture row whose snippet overflows two lines, so the
//      clamp must actually be doing work.
//   2. Assert scrollHeight > clientHeight (something is being clipped —
//      catches a 3+ line cell that just shows whatever fits without
//      clamping).
//   3. Assert clientHeight === lineHeight * 2 within rounding (catches
//      both the 1-line collapse and the 3-line leak).
//   4. Assert computed -webkit-line-clamp is "2" (catches a future
//      refactor that drops the CSS rule entirely).

test("Contents column clamps to exactly two lines with ellipsis", async ({
  page,
  request,
}) => {
  // Find a fixture row with a snippet long enough that a 2-line clamp
  // must actually truncate. 200 chars comfortably overflows two lines at
  // the column's typical width.
  const resp = await request.get("/applet/unified_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as {
    rows: { uuid: string; snippet: string | null }[];
  };
  const longRow = data.rows.find((r) => (r.snippet ?? "").length > 200);
  expect(
    longRow,
    "fixture must contain at least one row with a long snippet to exercise the clamp",
  ).toBeTruthy();

  await page.goto("/");
  await expect(
    page.locator('.ag-grid-scrolling-rows [role="row"]').first(),
  ).toBeVisible({ timeout: 10_000 });
  // A collapsed grid keeps its rows in the DOM but paints nothing, and
  // a nudge into a zero-height viewport scrolls nowhere. Assert the
  // paint first so that failure reads as the layout bug it is rather
  // than as a missing clamp element.
  await expectGridPainted(page.locator(".ag-root-wrapper").first(), "Explore grid");

  // Reading one cell means bringing the row *and* the Contents column
  // into view — the grid virtualizes both axes — and re-nudging until
  // the grid has actually rendered them, which is what
  // `actOnRowByUuid` is for. A single nudge followed by a plain wait
  // loses whenever the viewport does not end up where the call asked:
  // nothing re-asks, and the wait expires against a DOM that will
  // never contain the cell.
  const metrics = await actOnRowByUuid(
    page,
    longRow!.uuid,
    async (row) => {
      const clamp = row.locator(".datalib-clamp-2").first();
      await expect(clamp).toBeVisible({ timeout: 3_000 });
      return clamp.evaluate((el) => {
        const cs = getComputedStyle(el);
        return {
          clientHeight: el.clientHeight,
          scrollHeight: el.scrollHeight,
          lineHeightPx: parseFloat(cs.lineHeight),
          webkitLineClamp: cs.webkitLineClamp,
        };
      });
    },
    "snippet",
  );

  // The clamp is engaged: rendered height < natural height.
  expect(
    metrics.scrollHeight,
    "snippet must actually be clipped — pick a longer fixture if this fails",
  ).toBeGreaterThan(metrics.clientHeight);

  // Visible height is two lines, give or take sub-pixel rounding.
  const expectedTwoLines = metrics.lineHeightPx * 2;
  expect(metrics.clientHeight).toBeGreaterThanOrEqual(
    Math.floor(expectedTwoLines) - 1,
  );
  expect(metrics.clientHeight).toBeLessThanOrEqual(
    Math.ceil(expectedTwoLines) + 1,
  );

  // And the clamp property itself is what we expect (guards against a
  // future refactor that drops the rule).
  expect(metrics.webkitLineClamp).toBe("2");
});
