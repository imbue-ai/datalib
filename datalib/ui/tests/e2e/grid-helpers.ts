import { expect, type Page } from "@playwright/test";

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
