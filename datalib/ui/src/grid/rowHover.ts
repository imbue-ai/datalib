// The row under the pointer, lit across the whole grid. A grid with
// pinned columns draws each row twice, once per pane, and the theme's
// `.slick-row:hover` lights only the half the pointer is over; this
// marks both halves `datalib-row-hover` (styled in tableGrid.css).
import type { SlickGrid } from "@slickgrid-universal/common";

const HOVER = "datalib-row-hover";

function rowOf(el: Element | null): number | undefined {
  const node = el?.closest<HTMLElement>(".slick-row[data-row]");
  return node ? Number(node.dataset.row) : undefined;
}

export function hoverWholeRow(grid: SlickGrid) {
  const container = grid.getContainerNode();
  let hovered: number | undefined;
  let pointer: { x: number; y: number } | undefined;

  const paint = () => {
    for (const canvas of grid.getCanvases()) {
      for (const node of canvas.querySelectorAll<HTMLElement>(".slick-row[data-row]")) {
        node.classList.toggle(HOVER, Number(node.dataset.row) === hovered);
      }
    }
  };
  const hover = (row: number | undefined) => {
    if (row === hovered) return;
    hovered = row;
    paint();
  };

  container.addEventListener("mousemove", (e) => {
    pointer = { x: e.clientX, y: e.clientY };
    hover(rowOf(e.target as Element));
  });
  container.addEventListener("mouseleave", () => {
    pointer = undefined;
    hover(undefined);
  });
  // Scrolling moves rows under a still pointer, and the browser sends no
  // mouse event for it. The card lives in a shadow root, so ask that
  // root, not the document, what is under the pointer.
  grid.onScroll.subscribe(() => {
    if (!pointer) return;
    const root = container.getRootNode() as Document | ShadowRoot;
    hover(rowOf(root.elementFromPoint(pointer.x, pointer.y)));
  });
  // A row drawn after the pointer arrived — scrolled into view, or
  // redrawn by an update — has not been marked yet.
  grid.onRendered.subscribe(paint);
}
