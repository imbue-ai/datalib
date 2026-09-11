// The commit-history panel's rows: a tree, one store at the top with
// its commits under it newest first, and under each commit the tables
// it left behind, largest first. A store's commits are never
// interleaved with another's — two stores' clocks say nothing about
// each other, and a `checkpoint … blobs` between two real commits
// read as noise.

import type { HistoryCommit, HistoryTable, TreeHistory } from "@/api";

export type HistoryLevel = "store" | "commit" | "table";

export type HistoryRow = {
  /// Grid row id, and the last segment of `path`.
  key: string;
  /// Where the row hangs in the tree: `[store]`, `[store, commit]`,
  /// `[store, commit, table]`.
  path: string[];
  level: HistoryLevel;
  /// What the tree column shows: the store's file name, the commit's
  /// message, the table's name.
  label: string;
  /// The store's file name — `entities.doltlite_db`.
  store: string;
  /// The store, data-root-relative.
  storePath: string;
  /// The step that writes this store: the first two segments of
  /// `storePath`. What a commit's run log is filtered to.
  stepId: string;
  /// Commit rows only.
  hash: string | null;
  date: string | null;
  /// The run that made the commit, when the message names one — the
  /// job id, when the app ran it, so it is what the log is filed under.
  run: string | null;
  /// Rows across the data tables after the commit; a table row's own
  /// count. Null on a store row.
  rows: number | null;
  added: number | null;
  deleted: number | null;
  modified: number | null;
};

/// Every raw-store table has a `<table>_bookkeeping` sidecar holding
/// the fetch stamps for the same rows (`datalib/backend/etl/README.md`).
/// It moves in lockstep with its primary, so listing it would double
/// every count and say nothing new.
export function isSidecar(table: string): boolean {
  return table.endsWith("_bookkeeping");
}

/// The message with the step's ` run=<id>` stamp taken off: the id has
/// a column of its own, and the message is for reading.
export function messageWithoutRun(message: string): string {
  return message.replace(/\s+run=\S+\s*$/, "");
}

function storeName(storePath: string): string {
  return storePath.slice(storePath.lastIndexOf("/") + 1);
}

function stepOf(storePath: string): string {
  return storePath.split("/").slice(0, 2).join("/");
}

function tableRow(storePath: string, hash: string, t: HistoryTable): HistoryRow {
  const commitKey = `${storePath}@${hash}`;
  const key = `${commitKey}#${t.table}`;
  return {
    key,
    path: [storePath, commitKey, key],
    level: "table",
    label: t.table,
    store: storeName(storePath),
    storePath,
    stepId: stepOf(storePath),
    hash,
    date: null,
    run: null,
    rows: t.rows,
    added: t.added,
    deleted: t.deleted,
    modified: t.modified,
  };
}

function commitRows(storePath: string, c: HistoryCommit): HistoryRow[] {
  const data = c.tables.filter((t) => !isSidecar(t.table));
  const sum = (pick: (t: HistoryTable) => number) => data.reduce((n, t) => n + pick(t), 0);
  const key = `${storePath}@${c.hash}`;
  const commit: HistoryRow = {
    key,
    path: [storePath, key],
    level: "commit",
    label: messageWithoutRun(c.message),
    store: storeName(storePath),
    storePath,
    stepId: stepOf(storePath),
    hash: c.hash,
    date: c.date,
    run: c.run,
    rows: sum((t) => t.rows),
    added: sum((t) => t.added),
    deleted: sum((t) => t.deleted),
    modified: sum((t) => t.modified),
  };
  return [commit, ...data.map((t) => tableRow(storePath, c.hash, t))];
}

/// Stores in the order they arrived (path order), each one's commits
/// in the order the API walked them — HEAD first.
export function historyRows(histories: TreeHistory[]): HistoryRow[] {
  const out: HistoryRow[] = [];
  for (const h of histories) {
    for (const s of h.stores) {
      out.push({
        key: s.path,
        path: [s.path],
        level: "store",
        label: storeName(s.path),
        store: storeName(s.path),
        storePath: s.path,
        stepId: stepOf(s.path),
        hash: null,
        date: null,
        run: null,
        rows: null,
        added: null,
        deleted: null,
        modified: null,
      });
      for (const c of s.commits) out.push(...commitRows(s.path, c));
    }
  }
  return out;
}

/// The stores whose walk stopped at the limit, by path.
export function truncatedStores(histories: TreeHistory[]): string[] {
  return histories.flatMap((h) => h.stores.filter((s) => s.truncated).map((s) => s.path));
}
