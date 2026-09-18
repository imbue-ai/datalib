// A right-click menu whose entries depend on the row under the click,
// on a grid whose menu is a fixed list: a bank of slots, each showing
// whichever entry is at its index for the click in hand, and hidden
// otherwise. The grid copies its options on the way in, so the slots
// are made once and read their entry each time the menu opens.
import type { MenuCommandItem, MenuFromCellCallbackArgs } from "@slickgrid-universal/common";

export type MenuEntry = {
  name: string;
  /// Why the entry cannot be taken right now — shown on hover, and the
  /// entry is drawn disabled. Null when it can.
  disabled?: string | null;
  danger?: boolean;
  /// A line between groups of entries, nothing else.
  separator?: boolean;
  action?: () => void;
};

/// Slickgrid `commandItems` for a menu of up to `slots` entries, read
/// from `entries` on each opening. An entry past the bank's end is not
/// shown; size the bank for the longest menu.
export function menuSlots(
  slots: number,
  entries: (args: MenuFromCellCallbackArgs) => MenuEntry[],
): MenuCommandItem[] {
  const at = (args: unknown, i: number): MenuEntry | undefined =>
    entries(args as MenuFromCellCallbackArgs)[i];
  return Array.from({ length: slots }, (_, i) => ({
    command: `entry-${i}`,
    itemVisibilityOverride: (args) => at(args, i) !== undefined,
    itemUsabilityOverride: (args) => {
      const e = at(args, i);
      return !!e && !e.separator && !e.disabled;
    },
    slotRenderer: (_item, args) => {
      const e = at(args, i);
      const wrap = document.createElement("div");
      // The item lays its icon and text out itself; the wrapper only
      // exists because a renderer returns one element.
      wrap.style.display = "contents";
      if (!e || e.separator) {
        const line = document.createElement("div");
        line.className = "menu-slot-separator";
        wrap.appendChild(line);
        return wrap;
      }
      const icon = document.createElement("div");
      icon.className = "slick-menu-icon";
      icon.textContent = "◦";
      const text = document.createElement("span");
      text.className = "slick-menu-content";
      if (e.danger) text.classList.add("menu-danger");
      text.textContent = e.name;
      if (e.disabled) text.title = e.disabled;
      wrap.append(icon, text);
      return wrap;
    },
    action: (_e, args) => at(args, i)?.action?.(),
  }));
}
