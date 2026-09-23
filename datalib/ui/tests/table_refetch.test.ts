import { describe, expect, it } from "vitest";
import { refetchesOn } from "../src/cards/tableRefetch";
import type { RootEvent } from "../src/live";

const log: RootEvent = { kind: "table_changed", table: "log", chain: 1 };
const rows: RootEvent = { kind: "table_changed", table: "manage.rows" };
const index: RootEvent = { kind: "index_changed" };
const storage: RootEvent = { kind: "table_changed", table: "storage" };

describe("refetchesOn", () => {
  /// The loop that pegged a desktop app for hours: a Problems card
  /// refetched on the `log` frame its own request line caused.
  it("never refetches on a log frame", () => {
    for (const url of [
      "/applet/unified_index/problems?q=",
      "/api/manage/rows",
      "/api/remote_media/allow",
    ]) {
      expect(refetchesOn(url, log)).toBe(false);
    }
  });

  it("refetches a Problems card when the index commits", () => {
    const url = "/applet/unified_index/problems?q=source_id%3Agmail";
    expect(refetchesOn(url, index)).toBe(true);
    expect(refetchesOn(url, rows)).toBe(false);
  });

  it("refetches the Manage rows when they move", () => {
    expect(refetchesOn("/api/manage/rows", rows)).toBe(true);
    expect(refetchesOn("/api/manage/rows", index)).toBe(false);
  });

  it("refetches a table no frame names on any frame but the log's", () => {
    expect(refetchesOn("/api/remote_media/fetched", storage)).toBe(true);
    expect(refetchesOn("/api/remote_media/fetched", index)).toBe(true);
  });
});
