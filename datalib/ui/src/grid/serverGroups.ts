// A grouped search, grouped by the server: every group arrives with its
// true count and its newest row, and its rows are read a page at a time,
// only as it is opened and scrolled (grid/pagedWindow.ts, one window per
// group). The grid's own grouping still draws the groups; what it is
// handed is each group's loaded rows and, while it has more, a
// placeholder row that says so. Pure: GridCard runs the fetches.

import type { PagedWindow } from "./pagedWindow";

/// One group as `/search/groups` answers it.
export type ServerGroup<Row> = {
  values: (string | null)[];
  count: number;
  sample: Row;
};

/// A group's rows, read on by offset.
export type GroupWindow<Row> = PagedWindow<Row, number>;

/// How a grouping column reads its group's value off a row: a field, or
/// a function (a label, for an identity column).
export type Getter<Row> = string | ((row: Row) => unknown);

/// The group a placeholder stands for, on the placeholder row.
export const MORE = "__more";

export function groupKey(values: (string | null)[]): string {
  return JSON.stringify(values);
}

/// The window a group starts with: nothing read, its first page next.
export function unread<Row>(group: ServerGroup<Row>, at: string | null): GroupWindow<Row> {
  return { rows: [], total: group.count, next: 0, at, pending: null };
}

/// The row that stands for a group's rows not yet read: the group's
/// sample, so every grouping column reads the group's own value off it,
/// under an id of its own.
export function placeholderOf<Row extends { uuid: string }>(group: ServerGroup<Row>): Row {
  const key = groupKey(group.values);
  return { ...group.sample, uuid: `more:${key}`, [MORE]: key };
}

/// What the grid is handed: each group's rows read so far, then its
/// placeholder while it has more.
export function groupItems<Row extends { uuid: string }>(
  groups: ServerGroup<Row>[],
  windows: ReadonlyMap<string, GroupWindow<Row>>,
): Row[] {
  return groups.flatMap((g) => {
    const w = windows.get(groupKey(g.values))!;
    return w.next === null ? w.rows : [...w.rows, placeholderOf(g)];
  });
}

function valueOf<Row>(getter: Getter<Row>, row: Row): unknown {
  return typeof getter === "function" ? getter(row) : (row as Record<string, unknown>)[getter];
}

/// The true count behind every group row the grid draws, at every level,
/// under the key the grid's data view gives that group: the values its
/// getters read, joined the way it joins them. Two groups the server
/// tells apart can read the same (two sources with one name): their
/// counts add up, as their rows do.
export function countsByKey<Row>(
  groups: ServerGroup<Row>[],
  getters: Getter<Row>[],
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const g of groups) {
    let key = "";
    getters.forEach((getter, level) => {
      const value = valueOf(getter, g.sample);
      key = level === 0 ? `${value}` : `${key}:|:${value}`;
      counts.set(key, (counts.get(key) ?? 0) + g.count);
    });
  }
  return counts;
}
