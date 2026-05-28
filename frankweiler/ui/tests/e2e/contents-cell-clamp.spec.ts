import { test, expect } from "@playwright/test";

// What this test pins:
//   The Contents column in the search grid must render long snippets at
//   *exactly two lines* of text with an ellipsis cutting off the rest.
//   The row height is fixed (autoHeight is intentionally off — per-row
//   measurement was the dominant render cost on large result sets), so
//   getting the clamp wrong is visually ugly: either a stray partial
//   third line leaks out the bottom of the row, or the snippet
//   collapses to a single line and wastes half the row's vertical space.
//
// Why a test is warranted:
//   The clamp is implemented with -webkit-line-clamp, which is famously
//   finicky — it only acts on the element directly containing the text
//   (so it has to land on a div we render ourselves, not on AG Grid's
//   outer .ag-cell), and that element has to be width:100% or it
//   collapses to one line. We've now hit both failure modes once, so
//   the contract is worth pinning before a future refactor (different
//   cellRenderer, AG Grid upgrade, CSS-in-JS migration, etc.) silently
//   regresses one of them.
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
  const resp = await request.get("/api/search?q=&limit=2000");
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
    page.locator('.ag-center-cols-container [role="row"]').first(),
  ).toBeVisible({ timeout: 10_000 });

  // Scroll the long-snippet row into view (the grid is virtualized).
  await page.evaluate((uuid) => {
    const api = (
      window as unknown as {
        __fwGridApi?: {
          ensureNodeVisible: (
            comparator: (node: { data?: { uuid: string } }) => boolean,
            position?: string,
          ) => void;
        };
      }
    ).__fwGridApi;
    api?.ensureNodeVisible((n) => n.data?.uuid === uuid, "middle");
  }, longRow!.uuid);

  const clamp = page
    .locator(`[role="row"][row-id="${longRow!.uuid}"] .fw-clamp-2`)
    .first();
  await expect(clamp).toBeVisible();

  const metrics = await clamp.evaluate((el) => {
    const cs = getComputedStyle(el);
    return {
      clientHeight: el.clientHeight,
      scrollHeight: el.scrollHeight,
      lineHeightPx: parseFloat(cs.lineHeight),
      webkitLineClamp: cs.webkitLineClamp,
    };
  });

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
