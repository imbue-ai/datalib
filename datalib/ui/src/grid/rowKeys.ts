// Each rendered row of a grid stamped with its record's key as
// `data-key`, so a test — or anyone reading the DOM — can find a row by
// what it stands for; SlickGrid stamps nothing else naming the record
// on a row. Read off the canvas rather than the grid's row cache, which
// fills its cell lookups lazily and answers nothing for a row rendered
// past the view.
import type { SlickDataView, SlickGrid } from "@slickgrid-universal/common";

export function stampRowKeys(grid: SlickGrid, dataView: SlickDataView, keyOf: (item: unknown) => string) {
  grid.onRendered.subscribe(() => {
    for (const node of grid.getCanvasNode().querySelectorAll<HTMLElement>(".slick-row[data-row]")) {
      const item = dataView.getItem(Number(node.dataset.row));
      if (item) node.dataset.key = keyOf(item);
    }
  });
}
