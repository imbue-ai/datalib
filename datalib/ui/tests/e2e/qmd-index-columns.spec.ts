import { test, expect } from "@playwright/test";
import { searchAndSettle } from "./grid-helpers";

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

/// The groups the fixture embeds — `tests/fixtures/qmd_groups.bzl`, kept
/// in step by hand. Every group is indexed; only these carry vectors,
/// so a document outside them is the ❌ half of the Embedded column.
const EMBEDDED_GROUPS = new Set(["slack", "claude-api"]);

test("the applet reports the fixture's documents indexed, and embedded where the fixture embeds", async ({
  request,
}) => {
  const search = await request.get("/applet/unified_index/search?q=&limit=200");
  expect(search.ok(), `search API: HTTP ${search.status()}`).toBeTruthy();
  const rows = (
    (await search.json()) as { rows: { markdown_uuid: string | null; source_id: string }[] }
  ).rows;
  const uuids = [...new Set(rows.map((r) => r.markdown_uuid).filter(Boolean))];
  expect(uuids.length, "fixture rows must carry markdown_uuids").toBeGreaterThan(0);
  const groupOf = new Map(rows.filter((r) => r.markdown_uuid).map((r) => [r.markdown_uuid!, r.source_id]));

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
  expect(state.summary.embedded, "some of the fixture is embedded").toBeGreaterThan(0);
  expect(
    state.summary.embedded,
    "and some of it deliberately is not",
  ).toBeLessThan(state.summary.documents);

  // Every uuid we asked about comes back — no silent omissions.
  expect(Object.keys(state.docs).sort()).toEqual([...uuids].sort());

  const notIndexed = Object.entries(state.docs).filter(([, v]) => v.indexed !== true);
  expect(
    notIndexed,
    "every fixture document should hash-match a qmd `documents` row",
  ).toEqual([]);
  // The storage rows every source emits are filed under `datalib`,
  // but their document lives in the *source's* rendered tree and so in
  // its collection — which is where the embedding follows from. They
  // are left out of the line check rather than guessed at.
  const offTheLine = Object.entries(state.docs).filter(
    ([uuid, v]) =>
      groupOf.get(uuid) !== "datalib" && v.embedded !== EMBEDDED_GROUPS.has(groupOf.get(uuid)!),
  );
  expect(
    offTheLine,
    "a document should be embedded exactly when its group is one the fixture embeds",
  ).toEqual([]);
});

test("the columns are off by default and render check marks once shown", async ({
  page,
}) => {
  await page.goto("/");
  await page
    .locator('.ag-grid-scrolling-rows [role="row"]')
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
    .locator('.ag-grid-scrolling-rows [role="row"] [col-id="qmd_indexed"]')
    .first();
  await expect(firstIndexed).toHaveText(CHECK, { timeout: 15_000 });

  // Indexed: every rendered row is a check mark — the whole fixture is
  // keyword-indexed.
  const indexed = await page
    .locator('.ag-grid-scrolling-rows [role="row"] [col-id="qmd_indexed"]')
    .allInnerTexts();
  expect(indexed.length, "qmd_indexed cells rendered").toBeGreaterThan(0);
  expect(
    indexed.every((t) => t.trim() === CHECK),
    `qmd_indexed: every rendered cell should be a check mark, got ${JSON.stringify(indexed)}`,
  ).toBe(true);

  // Embedded: a check mark for a source the fixture embeds, a cross for
  // one it does not — one search per case, so both marks are seen to
  // render rather than one of them merely not contradicted.
  const embeddedCells = () =>
    page.locator('.ag-grid-scrolling-rows [role="row"] [col-id="qmd_embedded"]');
  await searchAndSettle(page, "source_id:slack");
  await expect(embeddedCells().first()).toHaveText(CHECK, { timeout: 15_000 });
  expect(
    (await embeddedCells().allInnerTexts()).every((t) => t.trim() === CHECK),
    "slack is embedded: every cell a check mark",
  ).toBe(true);
  await searchAndSettle(page, "source_id:notion");
  await expect(embeddedCells().first()).toHaveText(CROSS, { timeout: 15_000 });
  expect(
    (await embeddedCells().allInnerTexts()).every((t) => t.trim() === CROSS),
    "notion is indexed but not embedded: every cell a cross",
  ).toBe(true);
});
