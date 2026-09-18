// What a host of `TableGrid` can ask of it. Its own module because a
// `<script setup>` cannot export.
export type TableGridApi<R> = {
  /// Open the in-place editor on a cell.
  startEditing: (row: R, field: string) => void;
  /// The rows the grid has selected, in grid order.
  selectedRows: () => R[];
  refreshCells: (fields?: string[]) => void;
};
