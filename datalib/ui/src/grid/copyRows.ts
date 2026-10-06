// ⌘C (Ctrl+C elsewhere) on a grid puts its selected rows on the
// clipboard as tab-separated text: a header line, then a line per row,
// in the columns shown and the order they sit — what a spreadsheet or
// a bug report pastes. Each grid says what text a cell copies as, since
// what it draws (a relative time, a clipped hash) is not always it.
import type { Column, SlickEventData, SlickGrid } from "@slickgrid-universal/common";
import { copyToClipboard } from "@/clipboard";

/// A field as a spreadsheet reads it back: quoted, with its quotes
/// doubled, when it holds a tab, a line break or a quote.
export function tsvField(s: string): string {
  return /[\t\n\r"]/.test(s) ? `"${s.replaceAll('"', '""')}"` : s;
}

export function rowsAsTsv<T>(
  columns: readonly Column<T>[],
  rows: readonly T[],
  text: (column: Column<T>, row: T) => string,
): string {
  const header = columns.map((c) => (typeof c.name === "string" ? c.name : String(c.id)));
  const lines = rows.map((r) => columns.map((c) => text(c, r)));
  return [header, ...lines].map((cells) => cells.map(tsvField).join("\t")).join("\n");
}

/// Whether ⌘C copies the rows or is left to the browser. A text
/// selection wins while one row is selected: that is someone who
/// dragged across a message. With several, the rows win, because the
/// shift-click that selected them also stretched a text selection
/// across whatever lay between.
export function copiesRows(selectedRows: number, hasTextSelection: boolean): boolean {
  if (selectedRows === 0) return false;
  return selectedRows > 1 || !hasTextSelection;
}

function isCopyKey(e: KeyboardEvent): boolean {
  return (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "c";
}

/// Takes the grid's copy key. `recordAt` is the record at a row index,
/// or null for a row that is none (a group row, a placeholder).
export function copySelectedRowsOnKey<T>(
  grid: SlickGrid,
  recordAt: (row: number) => T | null,
  text: (column: Column<T>, row: T) => string,
) {
  grid.onKeyDown.subscribe((e: SlickEventData) => {
    const key = e.getNativeEvent<KeyboardEvent>();
    if (!key || !isCopyKey(key)) return;
    // The grid lists them in the order they were picked.
    const rows = [...grid.getSelectedRows()]
      .sort((a, b) => a - b)
      .map(recordAt)
      .filter((r): r is T => r != null);
    const hasTextSelection = String(window.getSelection() ?? "") !== "";
    if (!copiesRows(rows.length, hasTextSelection)) return;
    // Ahead of the grid's own Ctrl+C, which copies the one active cell.
    e.stopImmediatePropagation();
    key.preventDefault();
    const columns = grid.getVisibleColumns() as Column<T>[];
    void copyToClipboard(rowsAsTsv(columns, rows, text));
  });
}
