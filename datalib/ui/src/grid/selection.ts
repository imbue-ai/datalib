// Which records a selection change newly picked. A grid reports its
// selection as row indexes, and a refresh re-selects the same records
// at their new indexes: compared by index, a record that only moved
// reads as newly picked.

export function newlyPicked<T>(
  before: ReadonlySet<string>,
  now: readonly T[],
  keyOf: (row: T) => string,
): { picked: T[]; selected: Set<string> } {
  return {
    picked: now.filter((r) => !before.has(keyOf(r))),
    selected: new Set(now.map(keyOf)),
  };
}

/// The records a selection keeps across a new answer to the grid: a
/// re-sort, another search, a refresh. A change that adds to the
/// selection (⌘, Ctrl or Shift held) keeps the records it had that are
/// not among the rows held, to be selected again when they turn up; any
/// other change is the whole selection.
export function keptSelection(
  kept: ReadonlySet<string>,
  now: readonly string[],
  held: (id: string) => boolean,
  adds: boolean,
): Set<string> {
  if (!adds) return new Set(now);
  return new Set([...[...kept].filter((id) => !held(id)), ...now]);
}

/// The rows to select for `kept` among the rows held, in row order; null
/// when the grid's selection already is that.
export function rowsToRestore(
  kept: ReadonlySet<string>,
  rowOf: (id: string) => number | undefined,
  selected: readonly number[],
): number[] | null {
  const rows = [...kept]
    .map(rowOf)
    .filter((r): r is number => r != null)
    .sort((a, b) => a - b);
  const already = new Set(selected);
  return rows.length === already.size && rows.every((r) => already.has(r)) ? null : rows;
}
