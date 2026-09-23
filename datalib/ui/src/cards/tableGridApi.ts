// What a host of `TableGrid` can ask of it. Its own module because a
// `<script setup>` cannot export.
export type TableGridApi<R> = {
  /// Open the in-place editor on a cell.
  startEditing: (row: R, field: string) => void;
  /// The rows the grid has selected, in grid order.
  selectedRows: () => R[];
  /// Redraw the named columns in place, every row; the cell being
  /// edited is left alone.
  refreshCells: (fields: string[]) => void;
};
