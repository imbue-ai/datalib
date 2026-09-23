// What a person has done to a grid's columns — dragged a width, moved
// a column — carried onto a fresh set of definitions, so a producer
// re-declaring its columns does not undo it. Pure: columns in, columns
// out; the grid applies the result.

/// A column narrower than this reads as nothing at all.
export const MIN_COLUMN_WIDTH = 40;

interface Laid {
  id: string | number;
  width?: number;
}

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
