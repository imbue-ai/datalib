// The toolbar's sync indicator: what it counts and what its menu lists.
import { describe, expect, it } from "vitest";
import type { ManageRow } from "../src/api";
import { pillLabel, syncingGroups } from "../src/components/syncPill";

function row(over: Partial<ManageRow> & { id: string }): ManageRow {
  return {
    kind: "group",
    name: { id: over.id, label: over.id },
    type: { id: "slack", label: "Slack" },
    queue: { value: null, unit: "count" },
    eta: { value: null, unit: "seconds" },
    stop_request_ids: [],
    ...over,
  } as ManageRow;
}

describe("syncingGroups", () => {
  it("lists the groups a request has work left in, sources first", () => {
    const groups = syncingGroups([
      row({ id: "unified_index", type: null, stop_request_ids: ["r1", "r2"] }),
      row({ id: "bridge_log", stop_request_ids: [] }),
      row({
        id: "starfleet_mail",
        name: { id: "starfleet_mail", label: "Starfleet mail" },
        queue: { value: 1204, unit: "count" },
        eta: { value: 180, unit: "seconds" },
        stop_request_ids: ["r1"],
      }),
      row({ id: "starfleet_mail/ingest", kind: "step", stop_request_ids: ["r1"] }),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["starfleet_mail", "unified_index"]);
    expect(groups[0].name).toBe("Starfleet mail");
    expect(groups[0].progress).toBe(`${(1204).toLocaleString()} to go · 3 min left`);
    expect(groups[1].progress).toBe("");
    expect(groups[1].requestIds).toEqual(["r1", "r2"]);
  });
});

describe("pillLabel", () => {
  const source = (id: string) => syncingGroups([row({ id, stop_request_ids: ["r"] })])[0];
  const index = syncingGroups([
    row({
      id: "unified_index",
      name: { id: "unified_index", label: "Unified Index" },
      type: null,
      stop_request_ids: ["r"],
    }),
  ])[0];

  it("counts the sources, not the index their syncs reach", () => {
    expect(pillLabel([source("a"), index])).toBe("Syncing 1 source");
    expect(pillLabel([source("a"), source("b"), index])).toBe("Syncing 2 sources");
  });

  it("names the index when it is all that is left", () => {
    expect(pillLabel([index])).toBe("Syncing Unified Index");
  });

  it("says only that something syncs before the rows are known", () => {
    expect(pillLabel([])).toBe("Syncing");
  });
});
