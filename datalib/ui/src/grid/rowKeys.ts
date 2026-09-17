// Each rendered row of a grid stamped with its record's key as
// `data-key`, so a test — or anyone reading the DOM — can find a row by
// what it stands for; SlickGrid stamps nothing else naming the record
// on a row.
import type { SlickDataView, SlickGrid } from "@slickgrid-universal/common";

export function stampRowKeys(grid: SlickGrid, dataView: SlickDataView, keyOf: (item: unknown) => string) {
  grid.onRendered.subscribe((_e, args) => {
    const columns = grid.getColumns().length;
    for (let row = args.startRow; row <= args.endRow; row++) {
      const item = dataView.getItem(row);
      if (!item) continue;
      for (let cell = 0; cell < columns; cell++) {
        const node = grid.getCellNode(row, cell);
        if (node) {
          node.parentElement?.setAttribute("data-key", keyOf(item));
          break;
        }
      }
    }
  });
}
