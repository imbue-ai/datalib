// The rules for a group or step chip: what it shows, what its hover and
// copy say, and what its menu and double-click open.

import { describe, expect, it } from "vitest";

import {
  browseQuery,
  entityCardSource,
  entityCopyText,
  entityLook,
  entityMenu,
  entityTitle,
  movesEntities,
  type EntityView,
} from "./entities";

const SLACK = "datalib:group/slack";
const INGEST = "datalib:step/slack/ingest";
const view = (
  label: string,
  icon: string,
  key: string,
  detail: string | null = null,
): EntityView => ({
  label,
  icon,
  detail: "Slack",
  status: {
    key,
    label: key === "failed" ? "Failed" : "Succeeded",
    at: null,
    detail,
  } as EntityView["status"],
});

describe("entityLook", () => {
  /** Until the answer lands the chip draws what the producer sent; then
   *  the name the config gives it now, which a rename changes. */
  it("draws the producer's name and mark until the answer, then the answer's", () => {
    const before = entityLook(SLACK, "Slack", undefined, "slack");
    expect(before.text).toBe("Slack");
    expect(before.icon).toBe("slack");
    expect(before.classes).toEqual(["handle-chip", "entity-chip"]);
    const after = entityLook(SLACK, "Slack", view("Work Slack", "slack", "succeeded"), "slack");
    expect(after.text).toBe("Work Slack");
    expect(after.ariaLabel).toBe("Work Slack, source, Succeeded");
  });

  it("marks a status worth noticing, and only that", () => {
    expect(
      entityLook(INGEST, "", view("Work Slack · Ingest", "step:ingest", "failed"), null).classes,
    ).toContain("entity-failed");
    expect(
      entityLook(INGEST, "", view("Work Slack · Ingest", "step:ingest", "succeeded"), null).classes,
    ).not.toContain("entity-succeeded");
    expect(entityLook(INGEST, "", undefined, null).text).toBe("slack/ingest");
  });
});

describe("a group or step chip's hover, copy, menu and card", () => {
  it("says what it is and where it stands", () => {
    const v = view("Work Slack · Ingest", "step:ingest", "failed", "token expired");
    const look = entityLook(INGEST, "", v, null);
    expect(entityTitle(INGEST, look, v)).toBe(
      "Work Slack · Ingest (slack/ingest)\nSlack\nFailed: token expired",
    );
  });

  it("copies its name with its URI", () => {
    expect(entityCopyText(SLACK, "Work Slack")).toBe("Work Slack (datalib:group/slack)");
  });

  it("opens a group's dashboard and a step's log", () => {
    expect(entityCardSource(SLACK)).toBe('syncDashboardView({"group":"slack"})');
    expect(entityCardSource(INGEST)).toBe('logView({"step":"slack/ingest","jumpToEnd":true})');
    expect(entityCardSource("mailto:riker@enterprise.org")).toBeNull();
  });

  it("offers browse for a group, not for a step", () => {
    expect(entityMenu(SLACK, "Work Slack").map((e) => e.id)).toEqual([
      "copy-name",
      "copy-id",
      "copy-both",
      "open",
      "browse",
    ]);
    expect(entityMenu(INGEST, "Work Slack · Ingest").map((e) => e.label)).toEqual([
      "Copy “Work Slack · Ingest”",
      "Copy slack/ingest",
      "Copy “Work Slack · Ingest (datalib:step/slack/ingest)”",
      "Show its log",
    ]);
    expect(browseQuery(SLACK)).toBe("source_id:slack is:document");
    expect(browseQuery(INGEST)).toBeNull();
  });
});

describe("movesEntities", () => {
  it("asks again on the frames that can move a status or a name, and on no other", () => {
    expect(movesEntities({ kind: "table_changed", table: "manage.rows" })).toBe(true);
    expect(movesEntities({ kind: "config_changed" })).toBe(true);
    expect(movesEntities({ kind: "table_changed", table: "log" })).toBe(false);
    expect(movesEntities({ kind: "index_changed" })).toBe(false);
  });
});
