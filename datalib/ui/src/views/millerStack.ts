// The miller stack as a value. What the URL says and what is on screen
// are both lists of columns; the decisions between them — which
// columns survive a navigation, what the page is called — are pure
// functions here, and MillerView applies them.
import { encodeColumns, type ColumnSpec } from "@/router/columns";

export type Slot = {
  id: string;
  source: string;
  // Opaque per-card state string (see HostCommands.setState).
  state: string;
  // Column width in px; null renders at DEFAULT_WIDTH until the user
  // drags the column's right edge. In the URL as a ratio of
  // DEFAULT_WIDTH.
  width: number | null;
  // Human-readable title the card set via ctx.setTitle, shown instead
  // of the source box when dev mode is off; null until compiled or
  // when the card never set one.
  title: string | null;
};

export const DEFAULT_WIDTH = 640;

// The stack "/" renders when the URL carries no columns.
export const DEFAULT_SPECS: ColumnSpec[] = [{ code: "gridView()", size: null, state: "" }];

export function isBlankSource(source: string): boolean {
  return source.trim() === "";
}

// Round a width ratio to two decimals for a terse, stable URL.
export function sizeRatio(width: number | null): number | null {
  return width == null ? null : Math.round((width / DEFAULT_WIDTH) * 100) / 100;
}

export function widthOf(spec: ColumnSpec): number | null {
  return spec.size != null ? spec.size * DEFAULT_WIDTH : null;
}

export function specsOf(list: Slot[]): ColumnSpec[] {
  return list
    .filter((s) => !isBlankSource(s.source))
    .map((s) => ({ code: s.source, size: sizeRatio(s.width), state: s.state }));
}

export function specOf(source: string): ColumnSpec {
  return { code: source, size: null, state: "" };
}

export function sameSpecs(a: ColumnSpec[], b: ColumnSpec[]): boolean {
  return (
    a.length === b.length &&
    a.every(
      (x, i) =>
        x.code === b[i].code && (x.size ?? null) === (b[i].size ?? null) && x.state === b[i].state,
    )
  );
}

// Keep "/" for the pristine default stack instead of writing it out.
export function pathFor(specs: ColumnSpec[]): string {
  return sameSpecs(specs, DEFAULT_SPECS) ? "/" : encodeColumns(specs);
}

// The stack after the URL changed under us (Back, Forward, a link, a
// hand-edited address). A column whose code and state are what the URL
// says stays mounted — Back from a document leaves the grid beside it
// untouched — and takes the URL's width; anything else is a fresh
// column, since a card reads its state once, when it mounts.
export function reconcile(
  slots: Slot[],
  specs: ColumnSpec[],
  fresh: (spec: ColumnSpec) => Slot,
): Slot[] {
  return specs.map((spec, i) => {
    const have = slots[i];
    if (have && have.source === spec.code && have.state === spec.state) {
      return { ...have, width: widthOf(spec) };
    }
    return fresh(spec);
  });
}

// What the browser calls the page: the newest column first, since a
// tab strip and a history menu cut a title from the right.
export function pageTitle(titles: string[]): string {
  return [...titles].reverse().concat("Datalib").join(" · ");
}
