// The two right-click entries every grid here offers on a cell: keep
// only the rows sharing its value, or drop them. How a grid applies the
// filter is its own business — the unified grid appends a token to its
// query, the log panel sets a column filter — so this holds just the
// wording, which is what makes the two feel like one control.
import type { DefaultMenuItem, MenuItemDef } from "ag-grid-community";

export function keepExcludeItems<T>(opts: {
  header: string;
  value: string;
  keep: () => void;
  exclude: () => void;
}): (MenuItemDef<T> | DefaultMenuItem)[] {
  return [
    { name: `Keep only ${opts.header}=${opts.value}`, action: opts.keep },
    { name: `Exclude all ${opts.header}=${opts.value}`, action: opts.exclude },
    "separator",
  ];
}
