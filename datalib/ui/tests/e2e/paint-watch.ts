// What a grid repaints while the test does nothing. A live refresh
// should change only what changed: redrawing every row, or scrolling,
// moves what is under the pointer, and a click or an open menu aimed at
// it lands somewhere else. Arm a watch over a window in which the test
// neither scrolls, sorts nor searches, since any of those may redraw
// everything by right.

import { expect, type Locator } from "@playwright/test";

export type Paints = {
  /// Moments that replaced every row on screen at once, where there
  /// were at least two.
  fullRedraws: number;
  /// Row elements replaced, one at a time or all together.
  rowsReplaced: number;
  /// How far the grid's viewport scrolled, in pixels.
  scrolledBy: number;
  /// The furthest the grid itself moved on the page, in pixels, read
  /// every frame: a banner that comes and goes above it moves every row
  /// under the pointer twice and leaves no trace at the end.
  movedBy: number;
};

type Watch = {
  fullRedraws: number;
  rowsReplaced: number;
  top: number;
  y: number;
  moved: number;
  frame?: number;
  observer?: MutationObserver;
};
type Watched = HTMLElement & { __paints?: Watch };

/// Start watching `grid` (any element around one SlickGrid); the
/// returned function stops the watch and says what it saw.
export async function watchPaints(grid: Locator): Promise<() => Promise<Paints>> {
  await grid.evaluate((el: Watched) => {
    const viewport = el.querySelector<HTMLElement>(".slick-viewport");
    const isRow = (n: Node) => n instanceof HTMLElement && n.classList.contains("slick-row");
    const state: Watch = {
      fullRedraws: 0,
      rowsReplaced: 0,
      top: viewport?.scrollTop ?? 0,
      y: el.getBoundingClientRect().top,
      moved: 0,
    };
    const sample = () => {
      const dy = Math.abs(el.getBoundingClientRect().top - state.y);
      if (dy > state.moved) state.moved = dy;
      state.frame = requestAnimationFrame(sample);
    };
    sample();
    const observer = new MutationObserver((records) => {
      // One callback is one task's worth of DOM changes; a canvas whose
      // every row went in it was redrawn whole.
      const byCanvas = new Map<Node, { removed: number; added: number }>();
      for (const r of records) {
        const c = byCanvas.get(r.target) ?? { removed: 0, added: 0 };
        c.removed += [...r.removedNodes].filter(isRow).length;
        c.added += [...r.addedNodes].filter(isRow).length;
        byCanvas.set(r.target, c);
      }
      for (const [canvas, c] of byCanvas) {
        if (c.removed === 0) continue;
        state.rowsReplaced += c.removed;
        const now = [...canvas.childNodes].filter(isRow).length;
        const before = now + c.removed - c.added;
        if (before >= 2 && c.removed >= before) state.fullRedraws++;
      }
    });
    for (const canvas of el.querySelectorAll(".grid-canvas")) {
      observer.observe(canvas, { childList: true });
    }
    state.observer = observer;
    el.__paints = state;
  });
  return () =>
    grid.evaluate((el: Watched) => {
      const p = el.__paints!;
      p.observer?.disconnect();
      if (p.frame !== undefined) cancelAnimationFrame(p.frame);
      const viewport = el.querySelector<HTMLElement>(".slick-viewport");
      return {
        fullRedraws: p.fullRedraws,
        rowsReplaced: p.rowsReplaced,
        scrolledBy: (viewport?.scrollTop ?? 0) - p.top,
        movedBy: Math.max(p.moved, Math.abs(el.getBoundingClientRect().top - p.y)),
      };
    });
}

/// The rules every live refresh keeps: no row redrawn wholesale, no
/// scroll nobody asked for, and the grid where it was on the page.
export function expectSanePaints(paints: Paints, what: string) {
  expect(paints.fullRedraws, `${what} redrew every row: ${JSON.stringify(paints)}`).toBe(0);
  expect(paints.scrolledBy, `${what} scrolled by itself: ${JSON.stringify(paints)}`).toBe(0);
  expect(paints.movedBy, `${what} moved on the page: ${JSON.stringify(paints)}`).toBe(0);
}
