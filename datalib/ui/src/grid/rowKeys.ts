// Each rendered row of a grid stamped with its record's key as
// `data-key`, so a test — or anyone reading the DOM — can find a row by
// what it stands for; SlickGrid stamps nothing else naming the record
// on a row. Read off the canvas rather than the grid's row cache, which
// fills its cell lookups lazily and answers nothing for a row rendered
// past the view. A grid with pinned columns draws each row twice, once
// per pane: both halves carry the key, and the pinned one says so.
import type { SlickDataView, SlickGrid } from "@slickgrid-universal/common";

export function stampRowKeys(
  grid: SlickGrid,
  dataView: SlickDataView,
  keyOf: (item: unknown) => string,
) {
  grid.onRendered.subscribe(() => {
    const frozen = (grid.getOptions().frozenColumn ?? -1) >= 0;
    for (const canvas of grid.getCanvases()) {
      const pinned = frozen && canvas.classList.contains("grid-canvas-left");
      for (const node of canvas.querySelectorAll<HTMLElement>(".slick-row[data-row]")) {
        const item = dataView.getItem(Number(node.dataset.row));
        if (item) node.dataset.key = keyOf(item);
        node.toggleAttribute("data-pinned", pinned);
      }
    }
  });
}
