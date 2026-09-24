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
