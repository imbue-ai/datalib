import { describe, expect, it } from "vitest";
import type { MenuFromCellCallbackArgs, SlickEventData } from "@slickgrid-universal/common";
import { menuSlots, type MenuEntry } from "./menu";

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
