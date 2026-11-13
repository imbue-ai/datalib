// The table idioms a grid's row selection leaves to us. The grid's own
// model already does the rest: a click selects one row, ⌘/Ctrl-click
// toggles one, Shift-click and Shift+↑/↓ select a range.
//
// A click is safe: it selects and nothing more. A row opens in full on
// a double-click or Enter, never because it was selected.
import type { SlickEventData, SlickGrid } from "@slickgrid-universal/common";

/// `recordAt` is the record at a row index, or null for a row that is
/// none (a group row, a placeholder). A double-click a handler before
/// this one took (a chip opening its own card) stops it from reaching
/// here with `stopImmediatePropagation`.
export function openRowsOn<T>(
  grid: SlickGrid,
  recordAt: (row: number) => T | null,
  open: (record: T) => void,
) {
  grid.onDblClick.subscribe((_e, args) => {
    const record = recordAt(args.row);
    if (record != null) open(record);
  });
  grid.onKeyDown.subscribe((e: SlickEventData) => {
    const key = e.getNativeEvent<KeyboardEvent>();
    if (!key || key.key !== "Enter" || key.metaKey || key.ctrlKey || key.altKey || key.shiftKey) {
      return;
    }
    const row = enterOpens(grid.getSelectedRows(), grid.getActiveCell()?.row ?? null);
    const record = row == null ? null : recordAt(row);
    if (record == null) return;
    key.preventDefault();
    open(record);
  });
}

/// What Enter opens: what is selected. The active cell, where the arrow
/// keys start, is a row index the grid leaves in place when rows load
/// above it, while the selection follows its record; of several selected
/// rows it says which one.
function enterOpens(selected: number[], active: number | null): number | null {
  if (selected.length === 1) return selected[0];
  return active != null && selected.includes(active) ? active : null;
}

function isSelectAllKey(e: KeyboardEvent): boolean {
  return (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "a";
}

/// ⌘A (Ctrl+A elsewhere) selects every row that is a record — every one
/// the grid holds, which on a paged grid is the pages read so far.
export function selectAllRowsOnKey<T>(grid: SlickGrid, recordAt: (row: number) => T | null) {
  grid.onKeyDown.subscribe((e: SlickEventData) => {
    const key = e.getNativeEvent<KeyboardEvent>();
    if (!key || !isSelectAllKey(key)) return;
    // Ahead of the browser's, which selects the page's text.
    key.preventDefault();
    const rows: number[] = [];
    for (let r = 0; r < grid.getDataLength(); r++) if (recordAt(r) != null) rows.push(r);
    grid.setSelectedRows(rows);
  });
}

type Keys = Pick<KeyboardEvent, "key" | "shiftKey" | "ctrlKey" | "metaKey" | "altKey">;

/// Where Tab sends the focus from a table's rows: past the whole table,
/// ahead or behind, rather than from cell to cell along one row. The
/// arrows move between rows. Null for any other key.
export function tabLeavesRows(key: Keys): "after" | "before" | null {
  if (key.key !== "Tab" || key.ctrlKey || key.metaKey || key.altKey) return null;
  return key.shiftKey ? "before" : "after";
}

/// The index a list's arrow keys move its pick to, or null for a key
/// that moves nothing. `at` is the pick's index, -1 for none.
export function stepThroughList(key: Keys, at: number, length: number): number | null {
  if (length === 0 || key.ctrlKey || key.metaKey || key.altKey || key.shiftKey) return null;
  switch (key.key) {
    case "ArrowDown":
      return Math.min(at + 1, length - 1);
    case "ArrowUp":
      return Math.max(at - 1, 0);
    case "Home":
      return 0;
    case "End":
      return length - 1;
    default:
      return null;
  }
}

/// With several rows selected, ↑ and ↓ move among them alone, as
/// Lightroom's filmstrip does: to the row before or after `active` that
/// is selected, staying put at either end, and the selection stays
/// whole. Null, leaving the key to the grid, when one row or none is
/// selected or for any other key.
export function stepThroughSelection(
  key: Keys,
  active: number | null,
  selected: readonly number[],
): number | null {
  if (selected.length < 2 || key.ctrlKey || key.metaKey || key.altKey || key.shiftKey) return null;
  const rows = [...selected].sort((a, b) => a - b);
  // Past either end: the active row when it is one of them, else the
  // nearest of them (a re-sort leaves the active row on another record).
  const atEnd = (nearest: number) => (active != null && rows.includes(active) ? active : nearest);
  if (key.key === "ArrowDown") {
    return rows.find((r) => r > (active ?? -1)) ?? atEnd(rows[rows.length - 1]);
  }
  if (key.key === "ArrowUp") {
    return [...rows].reverse().find((r) => r < (active ?? Infinity)) ?? atEnd(rows[0]);
  }
  return null;
}
