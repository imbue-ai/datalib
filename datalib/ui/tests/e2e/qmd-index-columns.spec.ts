import { GRID, actOnRowByUuid, EVERY_ROW, SEARCH_ROWS, type GridApi } from "./grid-helpers";
import { test, expect, type Page } from "@playwright/test";

// The grid's `Indexed` / `Embedded` columns, end to end against the
// fixture's real qmd index.
//
// Both columns ship hidden — they answer "why didn't search find X?",
// which is a question you go looking for. So the DOM test also pins the
// default, and that un-hiding them actually fetches: a version that
// gated the request on visibility but never re-ran it on un-hide would
// leave the columns blank forever, and would pass every assertion that
// only looked at the default state.

const CHECK = "✅";
const CROSS = "❌";

test("the applet reports the fixture's documents indexed and embedded", async ({ request }) => {
  const search = await request.get("/applet/unified_index/search?q=&limit=200");
  expect(search.ok(), `search API: HTTP ${search.status()}`).toBeTruthy();
  const rows = ((await search.json()) as { rows: { markdown_uuid: string | null }[] }).rows;
  const uuids = [...new Set(rows.map((r) => r.markdown_uuid).filter(Boolean))];
  expect(uuids.length, "fixture rows must carry markdown_uuids").toBeGreaterThan(0);

  const resp = await request.post("/applet/unified_index/qmd_state", {
    data: { markdown_uuids: uuids },
  });
  expect(resp.ok(), `qmd_state: HTTP ${resp.status()}`).toBeTruthy();
  const state = (await resp.json()) as {
    index_present: boolean;
    summary: { documents: number; embedded: number };
    docs: Record<string, { indexed: boolean | null; embedded: boolean | null }>;
  };

  expect(state.index_present, "the e2e fixture root ships a qmd index").toBe(true);
  expect(state.summary.documents).toBeGreaterThan(0);
  expect(state.summary.embedded, "the fixture embeds everything it indexes").toBe(
    state.summary.documents,
  );

  // Every uuid we asked about comes back — no silent omissions.
  expect(Object.keys(state.docs).sort()).toEqual([...uuids].sort());

  const notIndexed = Object.entries(state.docs).filter(([, v]) => v.indexed !== true);
  expect(notIndexed, "every fixture document should hash-match a qmd `documents` row").toEqual([]);
  const notEmbedded = Object.entries(state.docs).filter(([, v]) => v.embedded !== true);
  expect(notEmbedded, "every fixture document should be embedded").toEqual([]);
});

test("the columns are off by default and render check marks once shown", async ({ page }) => {
  await page.goto(GRID);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });

  // Off by default. This is the assertion that fails if someone drops
  // `hidden` — an easy thing to lose in a column edit, and one
  // nothing else would notice.
  await expect(
    page.locator('.grid-box .slick-header-column[col-id="qmd_indexed"]'),
    "Indexed must be hidden until asked for",
  ).toHaveCount(0);
  await expect(page.locator('.grid-box .slick-header-column[col-id="qmd_embedded"]')).toHaveCount(
    0,
  );

  // …but the summary line is on screen regardless, which is how a user
  // discovers the columns exist at all.
  await expect(page.locator(".qmd-summary")).toContainText("documents searchable");

  // Turn them on the way the column picker does.
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.showColumns([
      "qmd_indexed",
      "qmd_embedded",
    ]),
  );

  await expect(page.locator('.grid-box .slick-header-column[col-id="qmd_indexed"]')).toBeVisible();
  await expect(page.locator('.grid-box .slick-header-column[col-id="qmd_embedded"]')).toBeVisible();

  // Showing a column is what triggers the per-document request, so the
  // cells start as the unknown em dash and resolve a beat later. Wait
  // for the resolution rather than asserting on the first paint —
  // which also pins that un-hiding actually fetches, instead of leaving
  // the columns permanently blank.
  const firstIndexed = page.locator('.grid-box .slick-row [col-id="qmd_indexed"]').first();
  await expect(firstIndexed).toHaveText(CHECK, { timeout: 15_000 });

  for (const colId of ["qmd_indexed", "qmd_embedded"]) {
    const cells = page.locator(`.grid-box .slick-row [col-id="${colId}"]`);
    const texts = await cells.allInnerTexts();
    expect(texts.length, `${colId} cells rendered`).toBeGreaterThan(0);
    expect(
      texts.filter((t) => t.trim() === CROSS),
      `${colId}: fixture rows must not report as missing from the index`,
    ).toEqual([]);
    expect(
      texts.every((t) => t.trim() === CHECK),
      `${colId}: every rendered cell should be a check mark, got ${JSON.stringify(texts)}`,
    ).toBe(true);
  }
});

// The columns ask only about the rows on screen and a margin around them.
// Rows far below the first screen get their answers once the grid is
// scrolled to them; a version that asked once on load would leave them
// as the unknown em dash for good.
test("rows scrolled to later get their check marks too", async ({ page }) => {
  await page.goto(GRID);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });
  await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.showColumns([
      "qmd_indexed",
      "qmd_embedded",
    ]),
  );
  await expect(page.locator('.grid-box .slick-row [col-id="qmd_indexed"]').first()).toHaveText(
    CHECK,
    { timeout: 15_000 },
  );

  // Whichever end of the rows held the grid did not open on: the first
  // request covered the rows around the opening view, not these. By
  // uuid, since rows loading at that end renumber the ones there.
  const openedAtTop = (await page.locator('.grid-box .slick-row[data-row="0"]').count()) > 0;
  const target = await page.evaluate((top) => {
    const a = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
    return a.uuidAt(top ? a.rows().length - 1 : 0)!;
  }, openedAtTop);
  await actOnRowByUuid(
    page,
    target,
    (row) => expect(row.locator('[col-id="qmd_indexed"]')).toHaveText(CHECK, { timeout: 3_000 }),
    "qmd_indexed",
  );
});

// The line under the grid in each of the three states a root's qmd index
// can be in. The fixture ships the third; the other two are its real
// answers with the index state rewritten.
test.describe("the search coverage line", () => {
  async function rewriteQmdState(
    page: Page,
    rewrite: (state: {
      index_present: boolean;
      summary: { documents: number; embedded: number };
    }) => void,
  ) {
    await page.route("**/applet/unified_index/qmd_state", async (route) => {
      const response = await route.fetch();
      const state = await response.json();
      rewrite(state);
      await route.fulfill({ response, json: state });
    });
  }

  // A rewrite still waiting on its `route.fetch()` when the page closes
  // throws outside any test and takes the worker down with it.
  test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

  test("with no index, says to sync, and free text says so without an error", async ({ page }) => {
    await rewriteQmdState(page, (state) => {
      state.index_present = false;
      state.summary = { documents: 0, embedded: 0 };
    });
    // Free text is answered as the applet does with no index, from the
    // unfiltered search's answer: asking the fixture's real qmd would
    // make this wait on a ranking it throws away.
    await page.route("**/applet/unified_index/search?**", async (route) => {
      const url = new URL(route.request().url());
      const q = url.searchParams.get("q") ?? "";
      if (q === "") return route.fallback();
      url.searchParams.delete("q");
      const response = await route.fetch({ url: url.toString() });
      const body = await response.json();
      body.rows = [];
      body.total = 0;
      body.next_offset = null;
      body.query_echo.free_text = q;
      body.query_echo.qmd_index_missing = true;
      await route.fulfill({ response, json: body });
    });
    // Every row: this test's free text is anything in the search bar.
    await page.goto(EVERY_ROW);
    await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 10_000 });
    await expect(page.locator(".qmd-summary")).toHaveText(
      "· search index not built yet — sync to build it",
    );

    await page.getByTestId("search-input").fill("enterprise");
    // The rewritten answer goes through the browser's request
    // interception, which WebKit has taken close to five seconds to
    // deliver on a loaded runner; the rows above get ten.
    await expect(page.locator(".qmd-unbuilt")).toHaveText(
      "Free-text search starts working once the first sync builds the search index.",
      { timeout: 15_000 },
    );
    await expect(page.locator(".qmd-error")).toHaveCount(0);
    await expect(page.getByText("no matches.")).toHaveCount(0);
  });

  test("with keyword search only, counts each index apart", async ({ page }) => {
    let documents = 0;
    await rewriteQmdState(page, (state) => {
      documents = state.summary.documents;
      state.summary.embedded = 0;
    });
    await page.goto(GRID);
    await expect(page.locator(".qmd-summary")).toHaveText(
      /^\s*· [\d,]+ documents searchable · 0 with semantic search$/,
    );
    expect(documents, "the fixture's keyword index holds documents").toBeGreaterThan(0);
    await expect(page.locator(".qmd-summary")).toHaveText(
      `· ${documents.toLocaleString("en-US")} documents searchable · 0 with semantic search`,
    );
  });

  test("with both, counts every document in each", async ({ page }) => {
    await page.goto(GRID);
    await expect(page.locator(".qmd-summary")).toHaveText(
      /^\s*· ([\d,]+) documents searchable · \1 with semantic search$/,
    );
  });
});
