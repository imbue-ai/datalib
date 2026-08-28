import { test, expect } from "@playwright/test";

// The grid's `Indexed` / `Embedded` columns, end to end against the
// fixture's real qmd index.
//
// The fixture indexes AND embeds every rendered document, so a healthy
// run shows ✅ in both columns for every row. That makes the negative
// case the interesting one to guard: if the hash join breaks — qmd
// changes how it hashes content, or `markdowns.md_path` stops
// resolving — every cell flips to ❌ and nothing else in the suite
// fails. Asserting "no ❌ anywhere" is what catches that.
//
// The endpoint assertion runs first and separately from the DOM one:
// when both fail, knowing whether the backend or the wiring broke is
// most of the debugging.
//
// Both columns ship hidden — they answer "why didn't search find X?",
// which is a question you go looking for. So the DOM test also pins the
// default, and that un-hiding them actually fetches: a version that
// gated the request on visibility but never re-ran it on un-hide would
// leave the columns blank forever, and would pass every assertion that
// only looked at the default state.

const CHECK = "✅";
const CROSS = "❌";

test("the applet reports the fixture's documents indexed and embedded", async ({
  request,
}) => {
  const search = await request.get("/applet/unified_index/search?q=&limit=200");
  expect(search.ok(), `search API: HTTP ${search.status()}`).toBeTruthy();
  const rows = ((await search.json()) as { rows: { markdown_uuid: string | null }[] })
    .rows;
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
  expect(
    state.summary.embedded,
    "the fixture embeds everything it indexes",
  ).toBe(state.summary.documents);

  // Every uuid we asked about comes back — no silent omissions.
  expect(Object.keys(state.docs).sort()).toEqual([...uuids].sort());

  const notIndexed = Object.entries(state.docs).filter(([, v]) => v.indexed !== true);
  expect(
    notIndexed,
    "every fixture document should hash-match a qmd `documents` row",
  ).toEqual([]);
  const notEmbedded = Object.entries(state.docs).filter(([, v]) => v.embedded !== true);
  expect(notEmbedded, "every fixture document should be embedded").toEqual([]);
});

test("the columns are off by default and render check marks once shown", async ({
  page,
}) => {
  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  // Off by default. This is the assertion that fails if someone drops
  // `hide: true` — an easy thing to lose in a colDef edit, and one
  // nothing else would notice.
  await expect(
    page.locator('.ag-header-cell[col-id="qmd_indexed"]'),
    "Indexed must be hidden until asked for",
  ).toHaveCount(0);
  await expect(
    page.locator('.ag-header-cell[col-id="qmd_embedded"]'),
  ).toHaveCount(0);

  // …but the summary line is on screen regardless, which is how a user
  // discovers the columns exist at all.
  await expect(page.locator(".qmd-summary")).toContainText(
    "documents searchable",
  );

  // Turn them on the way the Columns tool panel does.
  await page.evaluate(() => {
    const w = window as unknown as {
      __fwGridApi?: {
        applyColumnState: (p: {
          state: { colId: string; hide: boolean }[];
        }) => void;
      };
    };
    w.__fwGridApi!.applyColumnState({
      state: [
        { colId: "qmd_indexed", hide: false },
        { colId: "qmd_embedded", hide: false },
      ],
    });
  });

  await expect(
    page.locator('.ag-header-cell[col-id="qmd_indexed"]'),
  ).toBeVisible();
  await expect(
    page.locator('.ag-header-cell[col-id="qmd_embedded"]'),
  ).toBeVisible();

  // Showing a column is what triggers the per-document request, so the
  // cells start as the unknown em dash and resolve a beat later. Wait
  // for the resolution rather than asserting on the first paint —
  // which also pins that un-hiding actually fetches, instead of leaving
  // the columns permanently blank.
  const firstIndexed = page
    .locator('.ag-center-cols-container [role="row"] [col-id="qmd_indexed"]')
    .first();
  await expect(firstIndexed).toHaveText(CHECK, { timeout: 15_000 });

  for (const colId of ["qmd_indexed", "qmd_embedded"]) {
    const cells = page.locator(
      `.ag-center-cols-container [role="row"] [col-id="${colId}"]`,
    );
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
