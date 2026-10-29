import { describe, expect, it } from "vitest";
import { nowRow } from "./nowRow";

describe("nowRow", () => {
  /// The default order: ten rows held, two of them still to come, which
  /// the grid draws at the bottom; now is the row above them.
  it("puts the newest past row at the bottom, the rows to come below it", () => {
    expect(nowRow(2, 10, true)).toEqual({ row: 7, edge: "bottom" });
  });

  it("puts the newest past row at the top of a newest-first sort", () => {
    expect(nowRow(2, 10, false)).toEqual({ row: 2, edge: "top" });
  });

  /// Nothing to come is the newest row; nothing yet happened, the oldest.
  it("stays inside the rows held", () => {
    expect(nowRow(0, 10, true)).toEqual({ row: 9, edge: "bottom" });
    expect(nowRow(12, 10, true)).toEqual({ row: 0, edge: "bottom" });
    expect(nowRow(12, 10, false)).toEqual({ row: 9, edge: "top" });
    expect(nowRow(3, 0, false)).toBeNull();
  });
});
