import { describe, expect, it } from "vitest";
import { clockFaces, movedCells } from "./clockFaces";

const NOW = Date.parse("2026-09-23T12:00:00Z");
const iso = (ms: number) => new Date(ms).toISOString();
const COLS = { timestamps: ["last"], statuses: ["status"] };
const keyOf = (r: { key: string }) => r.key;

type Row = { key: string; last: string | null; status: unknown };

const row = (key: string, last: number | null, statusAt: number | null = null): Row => ({
  key,
  last: last == null ? null : iso(last),
  status: { key: "succeeded", label: "Succeeded", at: statusAt == null ? null : iso(statusAt) },
});

const moved = (rows: Row[], from: number, to: number) =>
  movedCells(clockFaces(rows, keyOf, COLS, from), clockFaces(rows, keyOf, COLS, to));

describe("the cells the clock moves", () => {
  /// The regression: the clock used to repaint every row whenever any
  /// one cell's text changed, rebuilding buttons under the pointer.
  it("names only the timestamp cell whose text changed", () => {
    const rows = [row("fresh", NOW - 59_000), row("old", NOW - 3 * 3_600_000)];
    expect(moved(rows, NOW, NOW + 2_000)).toEqual([{ key: "fresh", field: "last" }]);
  });

  /// A status cell draws when it got there beside its glyph, so it
  /// goes stale the way a timestamp does.
  it("names a status cell whose stamp now reads differently", () => {
    const rows = [row("s", null, NOW - 59_000)];
    expect(moved(rows, NOW, NOW + 2_000)).toEqual([{ key: "s", field: "status" }]);
  });

  it("names nothing when no cell reads differently", () => {
    const rows = [row("a", NOW - 10 * 60_000), row("b", null)];
    expect(moved(rows, NOW, NOW + 1_000)).toEqual([]);
  });

  it("leaves a row that came or went to the row's own paint", () => {
    const before = clockFaces([row("a", NOW - 59_000)], keyOf, COLS, NOW);
    const after = clockFaces([row("b", NOW - 59_000)], keyOf, COLS, NOW + 2_000);
    expect(movedCells(before, after)).toEqual([]);
  });
});
