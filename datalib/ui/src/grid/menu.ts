// A right-click menu whose entries depend on the row under the click,
// on a grid whose menu is a fixed list: a bank of slots, each showing
// whichever entry is at its index for the click in hand, and hidden
// otherwise. The grid copies its options on the way in, so the slots
// are made once and read their entry each time the menu opens.
//
// The entries are worked out once, as the menu opens, and every later
// call about that menu reads the same ones. The grid hands its callbacks
// the row's *index*, and a live refresh moves rows while a menu is open:
// worked out again at the click, an entry would act on whatever row now
// sits there, or be a different entry altogether.
import type {
  MenuCommandItem,
  MenuFromCellCallbackArgs,
  SlickEventData,
} from "@slickgrid-universal/common";

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

export type Box = { top: number; left: number; width: number; height: number };

/// Where a menu of `box` goes so all of it is inside `view`, `margin`
/// clear of each edge: moved rather than cut off, and only as tall as the
/// window when it is taller — past that, it scrolls. The grid flips a
/// menu above or below its row but never moves or shrinks it, so a long
/// menu on a low row in a short window ran off the top.
export function fitInView(
  box: Box,
  view: { width: number; height: number },
  margin = 8,
): { top: number; left: number; maxHeight: number | null } {
  const room = Math.max(0, view.height - 2 * margin);
  const height = Math.min(box.height, room);
  const clamp = (at: number, size: number, span: number) =>
    Math.max(margin, Math.min(at, span - margin - size));
  return {
    top: clamp(box.top, height, view.height),
    left: clamp(box.left, box.width, view.width),
    maxHeight: box.height > room ? room : null,
  };
}

/// `onAfterMenuShow` for a grid's `contextMenu` options: keeps the menu
/// inside the window. The grid calls it just before placing the menu,
/// so the fit waits for the placement.
export function onAfterMenuShowFit(_e: Event | SlickEventData, args: MenuFromCellCallbackArgs) {
  const uid = args.grid.getUID();
  queueMicrotask(() => {
    const el = document.body.querySelector<HTMLElement>(`.slick-context-menu.${uid}`);
    if (!el) return;
    const at = el.getBoundingClientRect();
    const fit = fitInView(at, { width: window.innerWidth, height: window.innerHeight });
    el.style.top = `${parseFloat(el.style.top || "0") + fit.top - at.top}px`;
    el.style.left = `${parseFloat(el.style.left || "0") + fit.left - at.left}px`;
    if (fit.maxHeight === null) return;
    // The menu's padding and border are measured in `at`, so the cap has to include them.
    el.style.boxSizing = "border-box";
    el.style.maxHeight = `${fit.maxHeight}px`;
  });
}

/// What `compute` gave when the menu last opened, for every call about
/// that opening. `onBeforeMenuShow` goes in the grid's `contextMenu`
/// options; `read` is what the items call.
export function perOpening<T>(compute: (args: MenuFromCellCallbackArgs) => T) {
  let held: { row?: number; cell?: number; value: T } | null = null;
  const open = (args: MenuFromCellCallbackArgs): T => {
    held = { row: args.row, cell: args.cell, value: compute(args) };
    return held.value;
  };
  return {
    onBeforeMenuShow: (_e: Event | SlickEventData, args: MenuFromCellCallbackArgs) => {
      open(args);
    },
    read: (args: MenuFromCellCallbackArgs): T =>
      held && held.row === args.row && held.cell === args.cell ? held.value : open(args),
  };
}

/// Slickgrid `contextMenu` options for a menu of up to `slots` entries,
/// read from `entries` as it opens. An entry past the bank's end is not
/// shown; size the bank for the longest menu.
export function menuSlots(
  slots: number,
  entries: (args: MenuFromCellCallbackArgs) => MenuEntry[],
): {
  commandItems: MenuCommandItem[];
  onBeforeMenuShow: ReturnType<typeof perOpening>["onBeforeMenuShow"];
  onAfterMenuShow: typeof onAfterMenuShowFit;
} {
  const opening = perOpening(entries);
  const at = (args: unknown, i: number): MenuEntry | undefined =>
    opening.read(args as MenuFromCellCallbackArgs)[i];
  const commandItems = Array.from({ length: slots }, (_, i): MenuCommandItem => ({
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
    action: (_e, args) => {
      const e = at(args, i);
      if (e && !e.disabled && !e.separator) e.action?.();
    },
  }));
  return {
    commandItems,
    onBeforeMenuShow: opening.onBeforeMenuShow,
    onAfterMenuShow: onAfterMenuShowFit,
  };
}
