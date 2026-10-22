import { describe, expect, it, vi } from "vitest";
import { logSource, personSource, searchSource, syncDashboardSource } from "./cardSources";
import {
  CHIP_ACTIONS,
  chipCardSource,
  chipElAt,
  chipMenuOf,
  chipOf,
  runChipAction,
  type Chip,
  type ChipHost,
} from "./chipActions";

const SLACK: Chip = { ref: { kind: "entity", uri: "datalib:group/slack" }, shownAs: "slack" };
const INGEST: Chip = {
  ref: { kind: "entity", uri: "datalib:step/slack/ingest" },
  shownAs: "ingest",
};
const RIKER: Chip = {
  ref: { kind: "person", handle: "email:riker@enterprise.org" },
  shownAs: "Will Riker",
};
const CONTACT: Chip = { ref: { kind: "contact", id: "c1" }, shownAs: "Will Riker" };

function host(extra: Omit<Partial<ChipHost>, "openCard"> = {}) {
  return { ...extra, openCard: vi.fn<(source: string) => void>() };
}

describe("chipMenuOf", () => {
  /** The grid draws a chip's entries in fixed slots of `CHIP_ACTIONS`, so
   *  a menu out of that order would lose entries there. */
  it("lists every kind's entries in the one order", () => {
    for (const [chip, canLink] of [
      [SLACK, false],
      [INGEST, false],
      [RIKER, false],
      [RIKER, true],
      [CONTACT, false],
    ] as const) {
      const ids = chipMenuOf(chip, canLink).map((e) => e.id);
      const order = ids.map((id) => CHIP_ACTIONS.indexOf(id));
      expect(order).toEqual([...order].sort((a, b) => a - b));
      expect(new Set(ids).size).toBe(ids.length);
    }
  });

  it("opens first wherever the chip has a card", () => {
    expect(chipMenuOf(SLACK, false)[0].id).toBe("open");
    expect(chipMenuOf(INGEST, false)[0].id).toBe("open");
    expect(chipMenuOf(RIKER, false)[0].id).toBe("open");
  });
});

describe("chipOf", () => {
  it("reads what an element names, from inside it too", () => {
    const a = document.createElement("a");
    a.className = "chip";
    a.dataset.entity = "datalib:group/slack";
    a.dataset.shownAs = "slack";
    const inner = document.createElement("span");
    a.append(inner);
    expect(chipElAt(inner)).toBe(a);
    expect(chipOf(a)).toEqual(SLACK);
    expect(chipElAt(document.createElement("span"))).toBeNull();
  });
});

describe("runChipAction", () => {
  it("opens the double-click's card, and a step's log", async () => {
    const h = host({ seenIn: "work_slack" });
    await runChipAction("open", SLACK, h);
    await runChipAction("open", RIKER, h);
    await runChipAction("open", INGEST, h);
    await runChipAction("log", INGEST, h);
    expect(h.openCard.mock.calls).toEqual([
      [syncDashboardSource({ group: "slack" })],
      [personSource("email:riker@enterprise.org", { seenIn: "work_slack" })],
      [syncDashboardSource({ group: "slack", step: "slack/ingest" })],
      [logSource({ step: "slack/ingest", jumpToEnd: true })],
    ]);
    expect(chipCardSource(CONTACT.ref, null)).toBeNull();
  });

  it("searches in the surface's own search where it has one, else beside it", async () => {
    const search = vi.fn();
    await runChipAction("browse", SLACK, host({ search }));
    await runChipAction("search", RIKER, host({ search }));
    expect(search.mock.calls).toEqual([
      ["source_id:slack is:document", "replace"],
      ["with:email:riker@enterprise.org", "narrow"],
    ]);

    const h = host();
    await runChipAction("browse", SLACK, h);
    expect(h.openCard).toHaveBeenCalledWith(searchSource("source_id:slack is:document"));
  });

  it("links a handle only where the surface hosts the popover", async () => {
    const editLink = vi.fn();
    await runChipAction("edit", RIKER, host({ editLink }));
    expect(editLink).toHaveBeenCalledOnce();
    await expect(runChipAction("edit", RIKER, host())).resolves.toBeUndefined();
  });
});
