// Which cells of a table the clock alone has changed. A `timestamp`
// cell reads "5 minutes ago", and a `status` cell says the same of when
// it got there, so both go stale with no new data. The grid repaints
// those cells, and only those, when their face here moves: repainting
// a row rebuilds its buttons under the pointer.
// A `timeseries` sparkline is not on the clock: it redraws when a new
// measurement arrives, like the status bar's.
import { formatRelative } from "@/config/timeFormat";

/// One string per clock-driven cell, keyed `<row key>\n<field>`. A cell
/// needs repainting when its string changes.
export type ClockFaces = Map<string, string>;

export type ClockColumns = {
  timestamps: string[];
  /// Status cells, whose stamp is the status's `at`.
  statuses: string[];
};

export function clockFaces<T extends Record<string, unknown>>(
  rows: T[],
  keyOf: (row: T) => string,
  cols: ClockColumns,
  now: number,
): ClockFaces {
  const faces: ClockFaces = new Map();
  for (const row of rows) {
    const key = keyOf(row);
    for (const f of cols.timestamps) {
      faces.set(`${key}\n${f}`, formatRelative((row[f] as string | null) ?? null, now));
    }
    for (const f of cols.statuses) {
      const at = (row[f] as { at?: string | null } | null)?.at ?? null;
      faces.set(`${key}\n${f}`, formatRelative(at, now));
    }
  }
  return faces;
}

/// The cells whose face differs between two readings, as row key and
/// field. A cell present in only one reading belongs to a row that came
/// or went, which the grid paints as a row, not here.
export function movedCells(
  before: ClockFaces,
  after: ClockFaces,
): { key: string; field: string }[] {
  const moved = [];
  for (const [cell, face] of after) {
    const was = before.get(cell);
    if (was === undefined || was === face) continue;
    const at = cell.lastIndexOf("\n");
    moved.push({ key: cell.slice(0, at), field: cell.slice(at + 1) });
  }
  return moved;
}
