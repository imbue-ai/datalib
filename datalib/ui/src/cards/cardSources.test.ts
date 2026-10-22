import { describe, expect, it } from "vitest";
import { sameCard, syncDashboardSource, syncDashboardStep } from "./cardSources";

describe("sameCard", () => {
  /** A step chip opened a second "Sync · slack" beside the one open:
   *  a group's dashboard is one card per window, whatever step it names. */
  it("makes a group's sync dashboard one card per window, whatever step it names", () => {
    const plain = sameCard(syncDashboardSource({ group: "slack" }));
    const atStep = sameCard(syncDashboardSource({ group: "slack", step: "slack/ingest" }));
    expect(plain).toEqual({ key: "syncDashboardView:slack", anywhere: true });
    expect(atStep).toEqual(plain);
    expect(sameCard(syncDashboardSource({ group: "github" })).key).not.toBe(plain.key);
  });

  it("leaves every other card the same only by its exact source, near its opener", () => {
    expect(sameCard('searchView({"q":"warp"})')).toEqual({
      key: 'searchView({"q":"warp"})',
      anywhere: false,
    });
    expect(sameCard("not a card source")).toEqual({ key: "not a card source", anywhere: false });
  });

  it("reads the step a dashboard's source names", () => {
    expect(syncDashboardStep(syncDashboardSource({ group: "slack", step: "slack/ingest" }))).toBe(
      "slack/ingest",
    );
    expect(syncDashboardStep(syncDashboardSource({ group: "slack" }))).toBeNull();
    expect(syncDashboardStep("garbage(")).toBeNull();
  });
});
