// Where now falls in a grid ordered newest first by a date, as a row of
// the grid and the edge of the viewport it goes at: the newest row that
// is not still to come, with what is to come beyond that edge. The
// server counts the rows still to come (`now_at`); this places that
// count once the grid has drawn the rows its own way up.

export type NowRow = { row: number; edge: "top" | "bottom" };

/// `nowAt` rows of the search are still to come and `held` are drawn:
/// upside down for the default order, newest at the bottom, else newest
/// at the top.
export function nowRow(nowAt: number, held: number, upsideDown: boolean): NowRow | null {
  if (held === 0) return null;
  const within = (i: number) => Math.min(Math.max(i, 0), held - 1);
  return upsideDown
    ? { row: within(held - 1 - nowAt), edge: "bottom" }
    : { row: within(nowAt), edge: "top" };
}
