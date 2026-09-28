import { describe, expect, it } from "vitest";
import { historyRows, type HistoryRow } from "./commitHistory";
import { addComparison, defaultPair, recordStore, selectedPair } from "./compareCommits";
import type { HistoryCommit, TreeHistory } from "@/api";

const commit = (hash: string, date: string): HistoryCommit => ({
  hash,
  parent: null,
  committer: "doltlite",
  date,
  message: `sync ${hash}`,
  run: null,
  tables: [],
});

const store = (path: string, hashes: string[]) => ({
  path,
  truncated: false,
  commits: hashes.map((h, i) => commit(h, `2026-09-0${9 - i}T12:00:00+00:00`)),
});

/// The history of a source whose download keeps two stores, and whose
/// render keeps one — newest first in each.
function enterprise(): HistoryRow[] {
  const h: TreeHistory = {
    tree: "enterprise",
    stores: [
      store("enterprise/ingest/blobs.doltlite_db", ["b2", "b1"]),
      store("enterprise/ingest/entities.doltlite_db", ["e3", "e2", "e1"]),
      store("enterprise/render_markdown/render.doltlite_db", ["r2", "r1"]),
    ],
  };
  return historyRows([h]);
}

const commitRow = (rows: HistoryRow[], hash: string) =>
  rows.find((r) => r.level === "commit" && r.hash === hash)!;

describe("compareCommits", () => {
  it("compares commits of the download's entities store, not its other stores", () => {
    expect(recordStore(enterprise(), "enterprise")).toBe("enterprise/ingest/entities.doltlite_db");
  });

  it("starts on the newest two commits, older as from", () => {
    const pair = defaultPair(enterprise(), "enterprise");
    expect(pair).toMatchObject({ from: { hash: "e2" }, to: { hash: "e3" } });
  });

  it("says why there is nothing to compare before a second sync", () => {
    const once = historyRows([
      { tree: "q", stores: [store("q/ingest/entities.doltlite_db", ["only"])] },
    ]);
    expect(defaultPair(once, "q")).toMatch(/synced once/);
    expect(defaultPair([], "q")).toMatch(/no synced data/);
  });

  it("orders a selected pair by the log, whichever was clicked first", () => {
    const rows = enterprise();
    const pair = selectedPair(rows, "enterprise", [commitRow(rows, "e1"), commitRow(rows, "e3")]);
    expect(pair).toMatchObject({ from: { hash: "e1" }, to: { hash: "e3" } });
  });

  /// Only the record store's commits are raw commits the diff render
  /// reads; a render or blob store's hash in the config fails its sync.
  it("refuses a selection that is not two commits of the record store", () => {
    const rows = enterprise();
    const e3 = commitRow(rows, "e3");
    expect(selectedPair(rows, "enterprise", [e3])).toBe("Select two commits to compare");
    expect(selectedPair(rows, "enterprise", [e3, commitRow(rows, "r2")])).toBe(
      "Compare two commits of enterprise/ingest/entities.doltlite_db",
    );
    expect(selectedPair(rows, "enterprise", [e3, commitRow(rows, "b1")])).toMatch(/^Compare two/);
    const tableRow = { ...e3, level: "table" as const, key: `${e3.key}#t` };
    expect(selectedPair(rows, "enterprise", [e3, commitRow(rows, "e2"), tableRow])).toBe(
      "Select two commits to compare",
    );
  });

  it("adds the comparison as a group with its render step", () => {
    const base = '[[groups]]\nid = "enterprise"\ntype = "slack"\n';
    const { text, renderId } = addComparison(base, {
      id: "enterprise-changes",
      name: "Enterprise · changes",
      source: "enterprise",
      from: "e2",
      to: "e3",
      maxDocuments: 50,
    });
    expect(renderId).toBe("enterprise-changes/render_markdown");
    expect(text).toContain('source = "enterprise"');
    expect(text).toContain('from = "e2"');
    expect(text).toContain('to = "e3"');
    expect(text).toContain("max_documents = 50");
  });
});
