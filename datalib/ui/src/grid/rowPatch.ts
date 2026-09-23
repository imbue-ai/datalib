// What a newer answer to the same query changes, row by row: which rows
// went, which changed, which are new. A grid that applies exactly that
// leaves every other row's element alone, and with it whatever the
// pointer was over.

export type RowPatch<T> = { removed: string[]; changed: T[]; added: T[] };

/// Each row's key and its JSON, as last handed to the grid.
export type Handed = Map<string, string>;

export function handedOf<T>(rows: T[], keyOf: (row: T) => string): Handed {
  return new Map(rows.map((r) => [keyOf(r), JSON.stringify(r)]));
}

export function patchRows<T>(
  before: Handed,
  after: T[],
  keyOf: (row: T) => string,
): { patch: RowPatch<T>; handed: Handed } {
  const handed: Handed = new Map();
  const changed: T[] = [];
  const added: T[] = [];
  for (const row of after) {
    const key = keyOf(row);
    const json = JSON.stringify(row);
    handed.set(key, json);
    const was = before.get(key);
    if (was === undefined) added.push(row);
    else if (was !== json) changed.push(row);
  }
  const removed = [...before.keys()].filter((k) => !handed.has(k));
  return { patch: { removed, changed, added }, handed };
}

export function isEmpty<T>(p: RowPatch<T>): boolean {
  return p.removed.length === 0 && p.changed.length === 0 && p.added.length === 0;
}
