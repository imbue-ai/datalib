// Where the miller row scrolls to show one column: the smallest move
// that brings the whole column into view, and the column's left edge
// when it is wider than the row (its header and first lines matter
// more than its far edge). Pure, so the rule is testable without a
// DOM; MillerView reads the spans off the elements and applies the
// result with scrollTo.

// A horizontal extent in the row's scroll coordinates.
export type Span = { start: number; width: number };

export function revealScrollLeft(view: Span, col: Span): number {
  if (col.width >= view.width || col.start < view.start) return col.start;
  const colEnd = col.start + col.width;
  const viewEnd = view.start + view.width;
  if (colEnd > viewEnd) return colEnd - view.width;
  return view.start;
}
