// A picture column dragged wider makes every row as tall as it is wide,
// so the picture grows with it; a grid has one row height, so the widest
// shown picture column sets it, and nothing goes below the grid's own.

export type SizedColumn = { id: string | number; hidden?: boolean; width?: number };

export function pictureRowHeight(
  columns: SizedColumn[],
  pictures: ReadonlySet<string>,
  base: number,
): number {
  const widths = columns
    .filter((c) => !c.hidden && pictures.has(String(c.id)))
    .map((c) => Math.round(c.width ?? 0));
  return Math.max(base, ...widths);
}
