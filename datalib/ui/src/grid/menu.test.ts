import { describe, expect, it } from "vitest";
import type { MenuFromCellCallbackArgs, SlickEventData } from "@slickgrid-universal/common";
import { fitInView, menuSlots, type MenuEntry } from "./menu";

const args = (row: number) => ({ row, cell: 0 }) as unknown as MenuFromCellCallbackArgs;
const event = {} as SlickEventData;

describe("a menu's entries", () => {
  /// The regression: an entry was looked up again at the click, by the
  /// row's index and the entry's slot. A row claimed by a sync between
  /// the opening and the click turned "Sync now" into "Stop the sync".
  it("are the ones the menu opened with, whatever changed before the click", () => {
    const ran: string[] = [];
    let claimed = false;
    const entries = (): MenuEntry[] => [
      claimed
        ? { name: "Stop the sync", action: () => ran.push("stop") }
        : { name: "Sync now", action: () => ran.push("sync") },
    ];
    const { commandItems, onBeforeMenuShow } = menuSlots(1, entries);

    onBeforeMenuShow(event, args(3));
    claimed = true;
    commandItems[0].action!(event, args(3) as never);

    expect(ran).toEqual(["sync"]);
  });

  it("are worked out again when the menu opens again", () => {
    const ran: string[] = [];
    let name = "first";
    const { commandItems, onBeforeMenuShow } = menuSlots(1, () => [
      { name, action: () => ran.push(name) },
    ]);
    onBeforeMenuShow(event, args(0));
    name = "second";
    onBeforeMenuShow(event, args(0));
    commandItems[0].action!(event, args(0) as never);
    expect(ran).toEqual(["second"]);
  });

  it("do nothing when the entry was disabled", () => {
    const ran: string[] = [];
    const { commandItems, onBeforeMenuShow } = menuSlots(1, () => [
      { name: "Sync now", disabled: "already syncing", action: () => ran.push("sync") },
    ]);
    onBeforeMenuShow(event, args(0));
    commandItems[0].action!(event, args(0) as never);
    expect(ran).toEqual([]);
  });
});

describe("a menu's place in the window", () => {
  const view = { width: 1000, height: 500 };

  /// The regression: a 14-entry menu opened above a low row in a short
  /// window started above the window's top, and its first entries
  /// ("Edit settings…") could not be reached, not even by scrolling.
  it("moves down when it opened above the window's top", () => {
    const fit = fitInView({ top: -120, left: 430, width: 330, height: 480 }, view);
    expect(fit).toEqual({ top: 8, left: 430, maxHeight: null });
  });

  it("moves up when it opened below the window's bottom", () => {
    const fit = fitInView({ top: 400, left: 10, width: 200, height: 300 }, view);
    expect(fit.top).toBe(192);
  });

  it("moves left when it opened past the window's right edge", () => {
    const fit = fitInView({ top: 50, left: 900, width: 330, height: 100 }, view);
    expect(fit.left).toBe(662);
  });

  it("stays put when it fits", () => {
    const fit = fitInView({ top: 50, left: 60, width: 200, height: 100 }, view);
    expect(fit).toEqual({ top: 50, left: 60, maxHeight: null });
  });

  it("is as tall as the window and scrolls when it is taller", () => {
    const fit = fitInView({ top: -300, left: 60, width: 200, height: 900 }, view);
    expect(fit).toEqual({ top: 8, left: 60, maxHeight: 484 });
  });
});
