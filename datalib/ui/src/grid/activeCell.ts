// The grid's active cell, where the arrow keys and Enter start, is a
// row index, and nothing moves it when rows move. After a refresh the
// selection has followed its record, and the grid re-finds the active
// cell at the old index, on whatever record now sits there. This moves
// it with its record, or clears it when the record is gone or has left
// the view: the keyboard is never left on a row the person did not pick.
import type { SlickDataView, SlickGrid } from "@slickgrid-universal/common";

export function keepActiveOnRecord(grid: SlickGrid, dataView: SlickDataView, mutate: () => void) {
  const active = grid.getActiveCell();
  // An open editor is the grid's own business; it closes or keeps its
  // row by the rules in TableGrid.
  const item = active && !grid.getCellEditor() ? dataView.getItem(active.row) : undefined;
  const id = (item as Record<string, unknown> | undefined)?.[dataView.getIdPropertyName()];
  mutate();
  if (!active || id === undefined) return;
  const row = dataView.getRowById(id as string);
  if (row === active.row) return;
  const { top, bottom } = grid.getViewport();
  if (row === undefined || row < top || row > bottom) {
    grid.resetActiveCell();
    return;
  }
  // In view, so this scrolls nothing; the event is suppressed because
  // the selection already followed the record on its own.
  grid.setActiveCell(row, active.cell, false, false, true);
}
