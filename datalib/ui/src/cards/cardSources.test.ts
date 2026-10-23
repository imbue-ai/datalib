import { describe, expect, it } from "vitest";
import { sameCard, sourceCardOpts, sourceSource, syncDashboardSource } from "./cardSources";

describe("sameCard", () => {
  /** A step chip opened a second "Sync · slack" beside the one open:
   *  a source's card is one per window, whatever step or tab it names,
   *  under its new name or its old one. */
  it("makes a source's card one per window, whatever it names and under either name", () => {
    const plain = sameCard(sourceSource({ group: "slack" }));
    expect(plain).toEqual({ key: "source:slack", anywhere: true });
    expect(sameCard(sourceSource({ group: "slack", step: "slack/ingest" }))).toEqual(plain);
    expect(sameCard(sourceSource({ group: "slack", tab: "sync" }))).toEqual(plain);
    expect(sameCard('syncDashboardView({"group":"slack"})')).toEqual(plain);
    expect(sameCard(sourceSource({ group: "github" })).key).not.toBe(plain.key);
  });

  it("leaves every other card the same only by its exact source, near its opener", () => {
    expect(sameCard('searchView({"q":"warp"})')).toEqual({
      key: 'searchView({"q":"warp"})',
      anywhere: false,
    });
    expect(sameCard("not a card source")).toEqual({ key: "not a card source", anywhere: false });
  });
});

describe("sourceCardOpts", () => {
  it("reads what a source's card source asks for", () => {
    expect(sourceCardOpts(sourceSource({ group: "slack", step: "slack/ingest" }))).toEqual({
      group: "slack",
      step: "slack/ingest",
      tab: undefined,
    });
    expect(syncDashboardSource({ group: "slack" })).toBe(
      'sourceView({"group":"slack","tab":"sync"})',
    );
    // The older name opens its Sync tab.
    expect(sourceCardOpts('syncDashboardView({"group":"slack"})')).toEqual({
      group: "slack",
      step: undefined,
      tab: "sync",
    });
    expect(sourceCardOpts('searchView({"q":"warp"})')).toBeNull();
    expect(sourceCardOpts("garbage(")).toBeNull();
  });
});
