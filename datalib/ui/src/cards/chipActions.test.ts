import { describe, expect, it, vi } from "vitest";
import { logSource, personSource, searchSource, sourceSource } from "./cardSources";
import type { ManageRow } from "@/api";
import {
  CHIP_ACTIONS,
  entityChipMenu,
  chipCardSource,
  chipElAt,
  chipMenuOf,
  chipOf,
  everythingWith,
  runChipAction,
  type Chip,
  type ChipAction,
  type ChipHost,
} from "./chipActions";
import { people, type NormalizedContact } from "./contacts";

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
      const order = ids.map((id) => CHIP_ACTIONS.indexOf(id as ChipAction));
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
      [sourceSource({ group: "slack" })],
      [personSource("email:riker@enterprise.org", { seenIn: "work_slack" })],
      [sourceSource({ group: "slack", step: "slack/ingest" })],
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

  /** "Everything with" on a linked handle searched that one handle, so it
   *  missed everything the contact's other handles were in. */
  it("searches everything with the contact a handle is linked to", async () => {
    const mine: NormalizedContact = {
      source_id: "datalib_contacts",
      key: "c1",
      kind: "person",
      names: ["Will Riker"],
      handles: [],
      org: null,
      title: null,
      seen: null,
    };
    const get = vi.spyOn(people, "get").mockReturnValue({ mine, sourceContacts: [] });
    const search = vi.fn();
    await runChipAction("search", RIKER, host({ search }));
    get.mockRestore();
    expect(search.mock.calls).toEqual([["with:contact:c1", "narrow"]]);
    expect(everythingWith("email:riker@enterprise.org", mine)).toBe("with:contact:c1");
    expect(everythingWith("email:riker@enterprise.org", null)).toBe(
      "with:email:riker@enterprise.org",
    );
  });

  it("links a handle only where the surface hosts the popover", async () => {
    const editLink = vi.fn();
    await runChipAction("edit", RIKER, host({ editLink }));
    expect(editLink).toHaveBeenCalledOnce();
    await expect(runChipAction("edit", RIKER, host())).resolves.toBeUndefined();
  });
});

/** A Sources row as the server sends it, for what a chip's menu reads. */
function manageRow(over: Partial<ManageRow> & { id: string; kind: ManageRow["kind"] }): ManageRow {
  return {
    name: { id: over.id, label: over.id },
    type: { id: "slack", label: "Slack" },
    group: over.kind === "step" ? over.id.split("/")[0] : null,
    function: over.kind === "step" ? over.id.split("/")[1] : null,
    actions: [
      { id: "browse", label: "Browse", enabled: true, hint: "", disabled_reason: null },
      { id: "sync", label: "Sync now", enabled: true, hint: "", disabled_reason: null },
    ],
    stop_request_ids: [],
    turned_off_by: null,
    status_from: over.kind === "group" ? `${over.id}/ingest` : null,
    reveal_blocked: null,
    reveal_path: `/root/${over.id}`,
    raw_store_path: null,
    params: {},
    ...over,
  } as ManageRow;
}

describe("entityChipMenu", () => {
  /** A group chip offered copy and open while its Sources row offered
   *  Sync, Stop, history and Reset: the chip's menu is now the row's. */
  it("is Open, then the group's Sources row, then the copies", () => {
    const labels = entityChipMenu(
      "datalib:group/slack",
      "Work Slack",
      manageRow({ id: "slack", kind: "group" }),
    ).map((e) => e.label);
    expect(labels[0]).toBe("Open this source");
    expect(labels).toContain("Sync now");
    expect(labels).toContain("Turn off");
    expect(labels).toContain("Show commit history");
    expect(labels).toContain("Reset…");
    // Its dashboard is the chip's Open, its id the chip's Copy; edits
    // need the config's text, which the Sources card holds.
    for (const gone of ["Show sync dashboard", "Copy id", "Edit settings…", "Rename…"]) {
      expect(labels).not.toContain(gone);
    }
    expect(labels.slice(-3)).toEqual([
      "Copy slack",
      "Copy “Work Slack”",
      "Copy “Work Slack (datalib:group/slack)”",
    ]);
  });

  it("offers Stop while a sync has work in it, and marks what stops or empties", () => {
    const entries = entityChipMenu(
      "datalib:group/slack",
      "Work Slack",
      manageRow({ id: "slack", kind: "group", stop_request_ids: ["r1"] }),
    );
    const stop = entries.find((e) => e.id === "row:stop");
    expect(stop?.danger).toBe(true);
    expect(entries.some((e) => e.id === "row:sync")).toBe(false);
    // A reset waits for the sync to stop, and says so.
    expect(entries.find((e) => e.id === "row:reset")?.disabled).toBe("Busy — stop the sync first");
  });

  it("takes the row's Browse and step log over the chip's own", () => {
    const ids = entityChipMenu(
      "datalib:step/slack/ingest",
      "Work Slack · Ingest",
      manageRow({ id: "slack/ingest", kind: "step" }),
    ).map((e) => e.id);
    expect(ids).toContain("row:log");
    expect(ids).not.toContain("log");
    expect(ids).not.toContain("browse");
  });

  it("is the chip's own menu until the row is known", () => {
    expect(entityChipMenu("datalib:group/slack", "Work Slack", undefined).map((e) => e.id)).toEqual(
      ["open", "copy-id", "copy-name", "copy-both", "browse"],
    );
  });
});
