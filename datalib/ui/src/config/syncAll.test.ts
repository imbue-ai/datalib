import { describe, expect, it } from "vitest";
import type { Action } from "../api";
import { syncAllButton } from "./syncAll";

const sync: Action = { id: "sync", label: "Sync now", enabled: true };
const stop = (enabled: boolean): Action => ({ id: "stop", label: "Stop", enabled });

describe("syncAllButton", () => {
  it("offers Sync everything while nothing syncs", () => {
    const b = syncAllButton([{ actions: [sync], stop_request_ids: [] }]);
    expect(b).toEqual({ glyph: "sync", label: "Sync everything", blocked: null, stops: [] });
  });

  it("is blocked with nothing configured", () => {
    expect(syncAllButton([]).blocked).toBe("Nothing configured yet.");
  });

  /// The index serves every source's sync, so its ids repeat the sources'.
  it("stops every open sync once, whichever rows name it", () => {
    const b = syncAllButton([
      { actions: [stop(true)], stop_request_ids: ["gmail"] },
      { actions: [sync], stop_request_ids: [] },
      { actions: [stop(true)], stop_request_ids: ["gmail", "slack"] },
    ]);
    expect(b.glyph).toBe("stop");
    expect(b.blocked).toBeNull();
    expect(b.stops).toEqual(["gmail", "slack"]);
  });

  it("reads Stopping, and takes no click, while every sync is already stopping", () => {
    const b = syncAllButton([{ actions: [stop(false)], stop_request_ids: ["gmail"] }]);
    expect(b.glyph).toBe("stop");
    expect(b.blocked).toMatch(/^Stopping everything/);
  });
});
