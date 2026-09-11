import { describe, expect, it } from "vitest";
import { historyRows, isSidecar, truncatedStores } from "./commitHistory";
import type { HistoryCommit, TreeHistory } from "@/api";

const commit = (
  hash: string,
  date: string,
  message: string,
  tables: HistoryCommit["tables"] = [],
): HistoryCommit => ({
  hash,
  parent: null,
  committer: "doltlite",
  date,
  message,
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
  it("folds the sidecars out of the totals and the table list", () => {
    const h: TreeHistory = {
      tree: "slack/ingest",
      stores: [
        {
          path: "slack/ingest/entities.doltlite_db",
          truncated: false,
          commits: [
            commit("aaa", "2026-09-08T20:54:34+00:00", "download slack", [
              table("messages", 1411, 1411),
              table("messages_bookkeeping", 1411, 1411),
              table("users", 413, 0, 2, 5),
              table("users_bookkeeping", 413, 0, 2, 5),
            ]),
          ],
        },
      ],
    };
    const [row] = historyRows(h);
    expect(row.key).toBe("slack/ingest/entities.doltlite_db@aaa");
    expect(row.store).toBe("entities.doltlite_db");
    expect(row.rows).toBe(1824);
    expect([row.added, row.deleted, row.modified]).toEqual([1411, 2, 5]);
    expect(row.tables).toBe("messages 1,411 (+1,411) · users 413 (−2 ~5)");
  });

  it("interleaves several stores newest first, keeping each store's walk order within a second", () => {
    const h: TreeHistory = {
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
    expect(historyRows(h).map((r) => r.hash)).toEqual(["e2", "b2", "b1", "e1"]);
    expect(truncatedStores(h)).toEqual(["slack/ingest/entities.doltlite_db"]);
  });

  it("names a sidecar by its suffix alone", () => {
    expect(isSidecar("messages_bookkeeping")).toBe(true);
    expect(isSidecar("bookkeeping_notes")).toBe(false);
  });
});
