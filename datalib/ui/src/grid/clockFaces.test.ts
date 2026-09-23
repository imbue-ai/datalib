import { describe, expect, it } from "vitest";
import { clockFaces, movedCells } from "./clockFaces";

const NOW = Date.parse("2026-09-23T12:00:00Z");
const iso = (ms: number) => new Date(ms).toISOString();
const COLS = { timestamps: ["last"], timeseries: ["bytes"], windowMs: 300_000, stepMs: 2_500 };
const keyOf = (r: { key: string }) => r.key;

type Row = { key: string; last: string | null; bytes: unknown };

const row = (key: string, last: number | null, sampleAts: number[] = []): Row => ({
  key,
  last: last == null ? null : iso(last),
  bytes: { value: 1, unit: "bytes", samples: sampleAts.map((at) => ({ at: iso(at), value: 1 })) },
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

  it("names nothing when no cell reads differently", () => {
    const rows = [row("a", NOW - 10 * 60_000), row("b", null)];
    expect(moved(rows, NOW, NOW + 1_000)).toEqual([]);
  });

  it("slides a sparkline with a step inside its window, a pixel at a time", () => {
    const rows = [row("live", null, [NOW - 60_000])];
    expect(moved(rows, NOW, NOW + 1_000)).toEqual([]);
    expect(moved(rows, NOW, NOW + 2_500)).toEqual([{ key: "live", field: "bytes" }]);
  });

  it("leaves a flat sparkline alone, after one last repaint as its step ages out", () => {
    const rows = [row("idle", null, [NOW - 299_000])];
    expect(moved(rows, NOW, NOW + 2_000)).toEqual([{ key: "idle", field: "bytes" }]);
    expect(moved(rows, NOW + 2_000, NOW + 60_000)).toEqual([]);
  });

  it("leaves a row that came or went to the row's own paint", () => {
    const before = clockFaces([row("a", NOW - 59_000)], keyOf, COLS, NOW);
    const after = clockFaces([row("b", NOW - 59_000)], keyOf, COLS, NOW + 2_000);
    expect(movedCells(before, after)).toEqual([]);
  });
});
