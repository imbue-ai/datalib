import { describe, expect, it, vi } from "vitest";
import { SlickEvent, SlickEventData, type SlickGrid } from "@slickgrid-universal/common";
import { openRowsOn, selectAllRowsOnKey } from "./rowIdioms";

type Line = { seq: number };

/// Rows 0 and 2 are lines; row 1 is a group row.
const rows: (Line | null)[] = [{ seq: 10 }, null, { seq: 12 }];
const lineAt = (row: number) => rows[row] ?? null;

/// The events and calls the helpers use, on a grid with no DOM.
function fakeGrid() {
  let active: { row: number; cell: number } | null = null;
  let selected: number[] = [];
  const grid = {
    onSelectedRowsChanged: new SlickEvent(),
    onDblClick: new SlickEvent(),
    onKeyDown: new SlickEvent(),
    getActiveCell: () => active,
    getDataLength: () => rows.length,
    getSelectedRows: () => selected,
    setSelectedRows: (r: number[]) => {
      selected = r;
    },
  };
  /// A click: the row is active and the one selected.
  const setActive = (row: number) => {
    active = { row, cell: 0 };
    selected = [row];
  };
  /// Where the active cell sits, apart from the selection.
  const moveActive = (row: number) => {
    active = { row, cell: 0 };
  };
  return { grid, slick: grid as unknown as SlickGrid, setActive, moveActive };
}

function key(k: string, mods: KeyboardEventInit = {}) {
  return new SlickEventData(new KeyboardEvent("keydown", { key: k, ...mods }));
}

function opened() {
  const { grid, slick, setActive, moveActive } = fakeGrid();
  const open = vi.fn();
  openRowsOn(slick, lineAt, open);
  return { grid, setActive, moveActive, open };
}

describe("what opens a row", () => {
  /// The regression: a click selected the line, and the selection opened
  /// it in a card beside the log. A click only selects.
  it("opens nothing when a click selects a line", () => {
    const { grid, open } = opened();
    grid.onSelectedRowsChanged.notify({ rows: [0] } as never);
    expect(open).not.toHaveBeenCalled();
  });

  it("opens nothing when the selection grows to several lines", () => {
    const { grid, open } = opened();
    grid.onSelectedRowsChanged.notify({ rows: [0] } as never);
    grid.onSelectedRowsChanged.notify({ rows: [0, 2] } as never);
    expect(open).not.toHaveBeenCalled();
  });

  it("opens the line under a double-click", () => {
    const { grid, open } = opened();
    grid.onDblClick.notify({ row: 2, cell: 0 } as never);
    expect(open).toHaveBeenCalledExactlyOnceWith({ seq: 12 });
  });

  it("opens the active line on Enter", () => {
    const { grid, setActive, open } = opened();
    setActive(0);
    grid.onKeyDown.notify({ row: 0, cell: 0 } as never, key("Enter"));
    expect(open).toHaveBeenCalledExactlyOnceWith({ seq: 10 });
  });

  it("opens nothing for a group row", () => {
    const { grid, setActive, open } = opened();
    grid.onDblClick.notify({ row: 1, cell: 0 } as never);
    setActive(1);
    grid.onKeyDown.notify({ row: 1, cell: 0 } as never, key("Enter"));
    expect(open).not.toHaveBeenCalled();
  });

  /// The active cell is a row index and stays put when rows load above
  /// it; the selection follows its record. Enter opened the row that had
  /// slid under the active cell.
  it("opens the one selected line on Enter, wherever the active cell sits", () => {
    const { grid, setActive, moveActive, open } = opened();
    setActive(2);
    moveActive(0);
    grid.onKeyDown.notify({ row: 0, cell: 0 } as never, key("Enter"));
    expect(open).toHaveBeenCalledExactlyOnceWith({ seq: 12 });
  });

  it("opens the active line of several selected, and nothing when it is not one of them", () => {
    const { grid, moveActive, open } = opened();
    grid.setSelectedRows([0, 2]);
    moveActive(2);
    grid.onKeyDown.notify({ row: 2, cell: 0 } as never, key("Enter"));
    expect(open).toHaveBeenCalledExactlyOnceWith({ seq: 12 });
    open.mockClear();
    moveActive(1);
    grid.onKeyDown.notify({ row: 1, cell: 0 } as never, key("Enter"));
    expect(open).not.toHaveBeenCalled();
  });

  it("leaves a modified Enter alone", () => {
    const { grid, setActive, open } = opened();
    setActive(0);
    grid.onKeyDown.notify({ row: 0, cell: 0 } as never, key("Enter", { shiftKey: true }));
    expect(open).not.toHaveBeenCalled();
  });
});

describe("select all", () => {
  it("selects every line on ⌘A or Ctrl+A, and no group row", () => {
    for (const mods of [{ metaKey: true }, { ctrlKey: true }]) {
      const { grid, slick } = fakeGrid();
      selectAllRowsOnKey(slick, lineAt);
      grid.onKeyDown.notify({ row: 0, cell: 0 } as never, key("a", mods));
      expect(grid.getSelectedRows()).toEqual([0, 2]);
    }
  });

  it("leaves a plain A alone", () => {
    const { grid, slick } = fakeGrid();
    selectAllRowsOnKey(slick, lineAt);
    grid.onKeyDown.notify({ row: 0, cell: 0 } as never, key("a"));
    expect(grid.getSelectedRows()).toEqual([]);
  });
});
