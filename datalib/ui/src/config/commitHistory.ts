// The commit-history panel's rows: one per commit across every store
// under the tree, newest first, with the per-table figures folded into
// the columns a person reads first.

import type { HistoryCommit, HistoryTable, TreeHistory } from "@/api";
import { compareStamps } from "@/config/timeFormat";

export type HistoryRow = {
  /// Grid row id: `<store>@<hash>`.
  key: string;
  /// The store's file name — `entities.doltlite_db` — since a tree can
  /// hold several and the message alone does not say which.
  store: string;
  /// The store, data-root-relative.
  storePath: string;
  hash: string;
  date: string;
  committer: string;
  message: string;
  /// Rows across every data table after this commit.
  rows: number;
  added: number;
  deleted: number;
  modified: number;
  /// The data tables, largest first, each with what this commit did to
  /// it: `messages 23,830 (+18,184) · users 413`.
  tables: string;
};

/// Every raw-store table has a `<table>_bookkeeping` sidecar holding
/// the fetch stamps for the same rows (`datalib/backend/etl/README.md`).
/// It moves in lockstep with its primary, so listing it would double
/// every count and say nothing new.
export function isSidecar(table: string): boolean {
  return table.endsWith("_bookkeeping");
}

const N = new Intl.NumberFormat();

function describeTable(t: HistoryTable): string {
  const delta = [
    t.added ? `+${N.format(t.added)}` : "",
    t.deleted ? `−${N.format(t.deleted)}` : "",
    t.modified ? `~${N.format(t.modified)}` : "",
  ]
    .filter(Boolean)
    .join(" ");
  return delta ? `${t.table} ${N.format(t.rows)} (${delta})` : `${t.table} ${N.format(t.rows)}`;
}

function rowOf(storePath: string, c: HistoryCommit): HistoryRow {
  const data = c.tables.filter((t) => !isSidecar(t.table));
  const sum = (pick: (t: HistoryTable) => number) => data.reduce((n, t) => n + pick(t), 0);
  return {
    key: `${storePath}@${c.hash}`,
    store: storePath.slice(storePath.lastIndexOf("/") + 1),
    storePath,
    hash: c.hash,
    date: c.date,
    committer: c.committer,
    message: c.message,
    rows: sum((t) => t.rows),
    added: sum((t) => t.added),
    deleted: sum((t) => t.deleted),
    modified: sum((t) => t.modified),
    tables: data.map(describeTable).join(" · "),
  };
}

/// Newest first across stores; within one second, the store's own
/// order, which is the walk from HEAD.
export function historyRows(h: TreeHistory): HistoryRow[] {
  const rows = h.stores.flatMap((s) => s.commits.map((c) => rowOf(s.path, c)));
  // `sort` is stable, and each store arrived newest first, so equal
  // stamps keep the walk order rather than shuffling within a second.
  return rows.sort((a, b) => compareStamps(b.date, a.date));
}

/// The stores whose walk stopped at the limit, by file name.
export function truncatedStores(h: TreeHistory): string[] {
  return h.stores.filter((s) => s.truncated).map((s) => s.path);
}
