// "Compare two versions" in the commit-history card: which two commits
// a comparison is between, and the config text that adds it. A
// comparison is a diff group over two commits of its source's raw
// store (docs/dev/plans/completed/diff_renderer.md), so only commits of
// that one store can be paired.

import type { HistoryRow } from "./commitHistory";
import { formatShortStamp, formatStamp } from "./timeFormat";
import {
  insertEntries,
  buildDiffSource,
  listGroups,
  listSteps,
  setQmdSteps,
  slugify,
  suggestId,
  wireIntoFanIns,
} from "./sourceSteps";

export type CommitPair = { store: string; from: HistoryRow; to: HistoryRow };

type Counts = { added: number; deleted: number; modified: number };

/// What the commits of a pair did to the record tables, summed commit by
/// commit: over the whole pair, and per table.
export type PairChanges = Counts & { tables: (Counts & { table: string })[] };

/// The store that holds the source's records: its download's
/// `entities` store when the download keeps several, else its only one.
export function recordStore(rows: HistoryRow[], source: string): string | null {
  const prefix = `${source}/ingest/`;
  const stores = rows.filter((r) => r.level === "store" && r.storePath.startsWith(prefix));
  const entities = stores.find((r) => r.store === "entities.doltlite_db");
  return (entities ?? stores[0])?.storePath ?? null;
}

function commitsOf(rows: HistoryRow[], store: string): HistoryRow[] {
  return rows.filter((r) => r.level === "commit" && r.storePath === store);
}

/// The newest two commits of the record store — "what the last sync
/// changed" — or why there are not two.
export function defaultPair(rows: HistoryRow[], source: string): CommitPair | string {
  const store = recordStore(rows, source);
  const commits = store ? commitsOf(rows, store) : [];
  if (!store || commits.length === 0) return "This source has no synced data yet.";
  if (commits.length === 1) {
    return "This source has synced once: there is nothing earlier to compare it with.";
  }
  return { store, to: commits[0], from: commits[1] };
}

/// The two selected commits as a pair, older first, or why they are
/// not one. `rows` is the history in its walk order, newest first.
export function selectedPair(
  rows: HistoryRow[],
  source: string,
  selected: HistoryRow[],
): CommitPair | string {
  const store = recordStore(rows, source);
  if (!store) return "This source has no synced data yet.";
  const commits = selected.filter((r) => r.level === "commit");
  if (commits.length !== 2 || commits.length !== selected.length) {
    return "Select two commits to compare";
  }
  if (commits.some((c) => c.storePath !== store)) {
    return `Compare two commits of ${store}`;
  }
  const order = commitsOf(rows, store).map((r) => r.key);
  const [a, b] = commits;
  return order.indexOf(a.key) < order.indexOf(b.key)
    ? { store, to: a, from: b }
    : { store, to: b, from: a };
}

/// Every commit after `from` up to and including `to`, each one's added,
/// deleted and modified counts summed on their own: a row added in one
/// commit and deleted in the next counts once as each.
export function pairChanges(rows: HistoryRow[], pair: CommitPair): PairChanges {
  const commits = commitsOf(rows, pair.store);
  const toAt = commits.findIndex((c) => c.key === pair.to.key);
  const fromAt = commits.findIndex((c) => c.key === pair.from.key);
  const inRange = new Set(commits.slice(toAt, fromAt).map((c) => c.key));
  const add = (into: Counts, r: HistoryRow) => {
    into.added += r.added ?? 0;
    into.deleted += r.deleted ?? 0;
    into.modified += r.modified ?? 0;
  };
  const total: PairChanges = { added: 0, deleted: 0, modified: 0, tables: [] };
  const byTable = new Map<string, Counts & { table: string }>();
  for (const r of rows) {
    if (r.level === "commit" && inRange.has(r.key)) add(total, r);
    if (r.level === "table" && r.records && inRange.has(r.path[1])) {
      let t = byTable.get(r.label);
      if (!t) byTable.set(r.label, (t = { table: r.label, added: 0, deleted: 0, modified: 0 }));
      add(t, r);
    }
  }
  total.tables = [...byTable.values()].filter((t) => t.added || t.deleted || t.modified);
  return total;
}

/// How one side of a pair reads: the minute it was made. The message is
/// the provider's own summary and says nothing about the comparison.
export function commitLabel(c: HistoryRow): string {
  return formatShortStamp(c.date);
}

/// The hover on one side: the exact second, the message and the hash,
/// for telling apart two commits made in one minute.
export function commitTooltip(c: HistoryRow): string {
  return [formatStamp(c.date), c.label, c.hash ?? ""].filter(Boolean).join("\n");
}

const COUNT_FMT = new Intl.NumberFormat();

/// What the pair's comparison will show, as record counts:
/// "3 added, 4 removed, 1 changed", leaving out what is zero.
export function changeSummary(c: Counts): string {
  const parts = (
    [
      [c.added, "added"],
      [c.deleted, "removed"],
      [c.modified, "changed"],
    ] as const
  )
    .filter(([n]) => n > 0)
    .map(([n, what]) => `${COUNT_FMT.format(n)} ${what}`);
  return parts.length ? parts.join(", ") : "No records changed";
}

/// The same counts table by table, for the summary's hover.
export function changeDetail(c: PairChanges): string {
  return c.tables.map((t) => `${t.table}: ${changeSummary(t)}`).join("\n");
}

/// Every id a new group may not take: the groups, and the groups the
/// steps name.
export function takenIds(text: string): Set<string> {
  return new Set([
    ...listGroups(text).map((g) => g.id),
    ...listSteps(text)
      .filter((s) => s.kind === "step")
      .map((s) => s.group ?? s.id),
  ]);
}

export function comparisonId(text: string, source: string, name: string): string {
  return suggestId(takenIds(text), slugify(name), `${source}-diff`);
}

/// The config with the comparison added: its group, its render step
/// wired into the fan-ins like any render, and searched. The render
/// step's id is what to sync next.
export function addComparison(
  text: string,
  opts: {
    id: string;
    name: string;
    source: string;
    from: string;
    to: string;
    maxDocuments: number;
  },
): { text: string; renderId: string } {
  const built = buildDiffSource(opts);
  let next = insertEntries(text, `${built.groupBody}\n\n${built.stepsBody}`);
  next = wireIntoFanIns(next, built.renderId);
  next = setQmdSteps(next, opts.id, "keyword_and_embed");
  return { text: next, renderId: built.renderId };
}
