import { expect, type Locator, type Page } from "@playwright/test";

// Scroll a (possibly virtualized-away) row into view via the grid api
// the GridCard exposes on window, then click it. Returns after the
// click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  const rowIndex = await page.evaluate(
    ({ uuid }) => {
      type Node = {
        rowIndex: number | null;
        data?: { uuid: string };
      };
      const w = window as unknown as {
        __fwGridApi?: {
          forEachNode: (cb: (n: Node) => void) => void;
          ensureNodeVisible: (n: Node, pos: "middle") => void;
        };
      };
      const api = w.__fwGridApi!;
      let found: number | null = null;
      api.forEachNode((node) => {
        if (node.data && node.data.uuid === uuid) {
          api.ensureNodeVisible(node, "middle");
          found = node.rowIndex;
        }
      });
      return found;
    },
    { uuid },
  );
  expect(rowIndex, `node for uuid=${uuid} found in grid`).not.toBeNull();
  await page
    .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
    .click();
}

// Assert that an AG Grid actually *painted*, not merely mounted.
//
// The failure this exists for: WebKit resolves a child's percentage
// `height` against the parent's *specified* height, so when the parent
// gets its height from flex resolution and declares none of its own,
// `height: 100%` computes to `auto` and the grid's root wrapper
// collapses to its border (~2px). Every row and header is still in the
// DOM — `.ag-row` locators match, `toHaveCount` passes — and nothing is
// on screen. Chromium resolves against the flexed height and looks
// fine, which is how the same bug shipped twice (`.grid` in
// cards/GridCard.ce.vue, `.m2-ag` in views/Manager2View.vue).
//
// So the assertion has to be geometric. 100px is comfortably above the
// collapsed states we've actually seen (2px of border; ~50px when only
// the row-group panel survives) and comfortably below any real grid,
// which fills a viewport-height flex column.
export async function expectGridPainted(
  grid: Locator,
  what: string,
  timeout = 10_000,
) {
  await expect(grid).toBeVisible({ timeout });
  await expect
    .poll(async () => (await grid.boundingBox())?.height ?? 0, {
      message: `${what}: .ag-root-wrapper must have real height, not a collapsed box`,
      timeout,
    })
    .toBeGreaterThan(100);
}
