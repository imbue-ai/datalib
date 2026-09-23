// Change a grid's rows and redraw only the ones that moved. The grid
// bundle answers any change in the row count by redrawing every row
// (`grid.invalidate()` in its `onRowCountChanged` handler). That suits
// a new dataset, and it is wrong for a refresh: every row element is
// replaced, and the click or menu aimed at one goes with it.
import type { SlickDataView, SlickGrid } from "@slickgrid-universal/common";

export function redrawChanged(grid: SlickGrid, dataView: SlickDataView, mutate: () => void) {
  const invalidate = grid.invalidate;
  let moved: number[] = [];
  const collect = (_e: unknown, args: { rows: number[]; calledOnRowCountChanged?: boolean }) => {
    // With the count unchanged, the bundle redraws these rows itself.
    if (args.calledOnRowCountChanged) moved = args.rows;
  };
  // For the length of `mutate`, the bundle's answer to a new count is
  // the count alone.
  grid.invalidate = () => grid.updateRowCount();
  dataView.onRowsChanged.subscribe(collect);
  try {
    mutate();
  } finally {
    grid.invalidate = invalidate;
    dataView.onRowsChanged.unsubscribe(collect);
  }
  grid.invalidateRows(moved);
  grid.render();
}
