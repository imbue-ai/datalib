import { describe, expect, it } from "vitest";
import { historyRows, isSidecar, messageWithoutRun, truncatedStores } from "./commitHistory";
import type { HistoryCommit, TreeHistory } from "@/api";

const commit = (
  hash: string,
  date: string,
  message: string,
  tables: HistoryCommit["tables"] = [],
  run: string | null = null,
): HistoryCommit => ({
  hash,
  parent: null,
  committer: "doltlite",
  date,
  message,
  run,
  tables,
});

const table = (table: string, rows: number, added = 0, deleted = 0, modified = 0) => ({
  table,
  rows,
  added,
  deleted,
  modified,
});

describe("historyRows", () => {
  it("hangs each commit under its store and each data table under its commit", () => {
    const h: TreeHistory = {
      tree: "slack/ingest",
      stores: [
        {
          path: "slack/ingest/entities.doltlite_db",
          truncated: false,
          commits: [
            commit(
              "aaa",
              "2026-09-08T20:54:34+00:00",
              "download slack: msgs=1 run=job-7",
              [
                table("messages", 1411, 1411),
                table("messages_bookkeeping", 1411, 1411),
                table("users", 413, 0, 2, 5),
                table("users_bookkeeping", 413, 0, 2, 5),
              ],
              "job-7",
            ),
          ],
        },
      ],
    };
    const rows = historyRows([h]);
    expect(rows.map((r) => [r.level, r.label])).toEqual([
      ["store", "entities.doltlite_db"],
      ["commit", "download slack: msgs=1"],
      ["table", "messages"],
      ["table", "users"],
    ]);
    const [store, c, messages] = rows;
    expect(store.path).toEqual(["slack/ingest/entities.doltlite_db"]);
    expect(store.rows).toBeNull();
    expect(c.path).toEqual(["slack/ingest/entities.doltlite_db", "slack/ingest/entities.doltlite_db@aaa"]);
    expect(c.stepId).toBe("slack/ingest");
    expect(c.run).toBe("job-7");
    expect(c.rows).toBe(1824);
    expect([c.added, c.deleted, c.modified]).toEqual([1411, 2, 5]);
    expect(messages.path[2]).toBe("slack/ingest/entities.doltlite_db@aaa#messages");
    expect([messages.rows, messages.added]).toEqual([1411, 1411]);
  });

  it("keeps each store's walk order and never interleaves stores", () => {
    const blobs: TreeHistory = {
      tree: "slack",
      stores: [
        {
          path: "slack/ingest/blobs.doltlite_db",
          truncated: false,
          commits: [
            commit("b2", "2026-09-08T20:54:34+00:00", "checkpoint blobs"),
            commit("b1", "2026-09-08T20:54:34+00:00", "Initialize data repository"),
          ],
        },
        {
          path: "slack/ingest/entities.doltlite_db",
          truncated: true,
          commits: [
            commit("e2", "2026-09-08T20:55:00+00:00", "download slack"),
            commit("e1", "2026-09-08T20:49:41+00:00", "schema: apply DDL"),
          ],
        },
      ],
    };
    const rows = historyRows([blobs]);
    expect(rows.map((r) => r.hash ?? r.label)).toEqual([
      "blobs.doltlite_db",
      "b2",
      "b1",
      "entities.doltlite_db",
      "e2",
      "e1",
    ]);
    expect(truncatedStores([blobs])).toEqual(["slack/ingest/entities.doltlite_db"]);
  });

  it("names a sidecar by its suffix alone", () => {
    expect(isSidecar("messages_bookkeeping")).toBe(true);
    expect(isSidecar("bookkeeping_notes")).toBe(false);
  });
});

describe("messageWithoutRun", () => {
  it("drops the trailing run stamp and nothing else", () => {
    expect(messageWithoutRun("download slack: msgs=4 run=0199-abc")).toBe("download slack: msgs=4");
    expect(messageWithoutRun("render slack: 2 document(s) run=j1 ")).toBe("render slack: 2 document(s)");
    expect(messageWithoutRun("schema: apply DDL")).toBe("schema: apply DDL");
    expect(messageWithoutRun("rerun=3 things")).toBe("rerun=3 things");
  });
});
