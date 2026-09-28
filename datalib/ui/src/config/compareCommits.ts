// "Compare two versions" in the commit-history card: which two commits
// a comparison is between, and the config text that adds it. A
// comparison is a diff group over two commits of its source's raw
// store (docs/dev/plans/completed/diff_renderer.md), so only commits of
// that one store can be paired.

import type { HistoryRow } from "./commitHistory";
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
