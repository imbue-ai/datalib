// How every grid here treats its columns: they keep the widths they
// are given or dragged to, through a resize, a refresh and a rebuild.
// The grid follows its box; its columns do not.
import type { GridOption } from "@slickgrid-universal/common";

/// Options every grid spreads into its own. slickgrid's default fits
/// the columns to the viewport on the first load, on every resize and
/// on every column update, which undoes any width a person dragged.
/// With no fit, a column needs no `minWidth` either: that only ever
/// stopped the fit squeezing it, and it stops a person too.
export const KEEP_COLUMN_WIDTHS = {
  enableAutoSizeColumns: false,
  autoFitColumnsOnFirstLoad: false,
} satisfies GridOption;

interface Laid {
  id: string | number;
  width?: number;
}

/// What a person did to the columns, carried onto a fresh set of
/// definitions so a producer re-declaring them does not undo it:
/// `fresh` in the order and at the widths of `current`, the columns the
/// grid shows now. A column `current` lacks keeps its declared width
/// and its declared place after the ones that are carried over.
export function carryLayout<C extends Laid>(fresh: C[], current: Laid[]): C[] {
  const place = new Map(current.map((c, i) => [c.id, i]));
  const width = new Map(current.map((c) => [c.id, c.width]));
  const rank = (c: C, i: number) => place.get(c.id) ?? current.length + i;
  return fresh
    .map((c, i) => ({ c, r: rank(c, i) }))
    .sort((a, b) => a.r - b.r)
    .map(({ c }) => {
      const w = width.get(c.id);
      return w == null ? c : { ...c, width: w };
    });
}
