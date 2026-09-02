import { expect, type Locator, type Page } from "@playwright/test";

// Scroll a (possibly virtualized-away) row into view via the grid api
// the GridCard exposes on window, and return its row index once the DOM
// node for it actually exists.
//
// The scroll and the wait cannot be one step. `ensureNodeVisible` moves
// the viewport, but AG Grid renders the newly-visible window on its own
// schedule, so the node at that index may not be in the DOM yet when
// `evaluate` returns. A plain locator wait on the index is not enough
// either: if the viewport did not end up where the call asked (a
// re-layout, a grid that has just been resized), waiting alone never
// converges and the click fails at its 30s default having never
// re-asked. So the nudge is inside the poll, and gets repeated until
// the row is there.
//
// This is a race the suite could always lose and mostly didn't; it
// surfaced when the specs started running four at a time and rendering
// got slower relative to the scroll.
async function scrollRowIntoView(page: Page, uuid: string): Promise<number> {
  // Annotated: `found` is only ever assigned inside the forEachNode
  // callback, so TypeScript infers the evaluate's return as plain
  // `null` and the cast below would be rejected as a mistake.
  const nudge = (): Promise<number | null> =>
    page.evaluate(
      ({ uuid }) => {
        type Node = {
          rowIndex: number | null;
          data?: { uuid: string };
        };
        const w = window as unknown as {
          __fwGridApi?: {
            forEachNode: (cb: (n: Node) => void) => void;
            ensureNodeVisible: (n: Node, pos: "middle") => void;
          };
        };
        const api = w.__fwGridApi!;
        let found: number | null = null;
        api.forEachNode((node) => {
          if (node.data && node.data.uuid === uuid) {
            api.ensureNodeVisible(node, "middle");
            found = node.rowIndex;
          }
        });
        return found;
      },
      { uuid },
    );

  const rowIndex = await nudge();
  expect(rowIndex, `node for uuid=${uuid} found in grid`).not.toBeNull();
  await expect
    .poll(
      async () => {
        await nudge();
        return page
          .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
          .count();
      },
      {
        timeout: 15_000,
        intervals: [100, 250, 250, 500],
        message: `row ${rowIndex} (uuid=${uuid}) never rendered after being scrolled to`,
      },
    )
    .toBeGreaterThan(0);
  return rowIndex as number;
}

// Scroll a (possibly virtualized-away) row into view, then click it.
// Returns after the click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  const rowIndex = await scrollRowIntoView(page, uuid);
  await page
    .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
    .click();
}

// Right-click a row located by uuid. Same virtualization dance as
// `clickRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  const rowIndex = await scrollRowIntoView(page, uuid);
  await page
    .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
    .click({ button: "right" });
  await expect(page.locator(".ag-menu")).toBeVisible({ timeout: 5_000 });
}

// Replace `navigator.clipboard.writeText` with a recorder, so a copy
// action can be asserted on without granting clipboard permissions
// (which differ per browser engine) or reading the real system
// clipboard (which would make the test order-dependent and flaky under
// parallelism). Returns a reader for whatever the page last copied.
export async function stubClipboard(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as { __copied?: string };
    w.__copied = undefined;
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: (t: string) => {
          w.__copied = t;
          return Promise.resolve();
        },
      },
    });
  });
  return async () =>
    page.evaluate(
      () => (window as unknown as { __copied?: string }).__copied ?? null,
    );
}

// Assert that an AG Grid actually *painted*, not merely mounted.
//
// The failure this exists for: WebKit resolves a child's percentage
// `height` against the parent's *specified* height, so when the parent
// gets its height from flex resolution and declares none of its own,
// `height: 100%` computes to `auto` and the grid's root wrapper
// collapses to its border (~2px). Every row and header is still in the
// DOM — `.ag-row` locators match, `toHaveCount` passes — and nothing is
// on screen. Chromium resolves against the flexed height and looks
// fine, which is how the same bug shipped twice (`.grid` in
// cards/GridCard.ce.vue, `.m2-ag` in views/Manager2View.vue).
//
// So the assertion has to be geometric. 100px is comfortably above the
// collapsed states we've actually seen (2px of border; ~50px when only
// the row-group panel survives) and comfortably below any real grid,
// which fills a viewport-height flex column.
export async function expectGridPainted(
  grid: Locator,
  what: string,
  timeout = 10_000,
) {
  await expect(grid).toBeVisible({ timeout });
  await expect
    .poll(async () => (await grid.boundingBox())?.height ?? 0, {
      message: `${what}: .ag-root-wrapper must have real height, not a collapsed box`,
      timeout,
    })
    .toBeGreaterThan(100);
}

// How long a search may take to come back.
//
// This is not a guess at network latency — it is the cost of the qmd
// daemon's model load. The applet holds a long-lived `qmd mcp` child
// (backend/unified_index/src/qmd/daemon.rs); the first free-text query
// after it starts pays for loading the embedding model, and every
// query after that is sub-second. The child is torn down and respawned
// whenever the applet restarts, which the http gateway does on any
// config change that touches its entry — so specs that rewrite
// config.toml re-arm that cost for whatever runs next. Under
// `--runs_per_test=N` every sandbox pays it at once.
//
// So: one named constant, big enough for a cold model load under
// contention, rather than a per-assertion number that silently becomes
// wrong when a query starts or stops routing through qmd.
export const SEARCH_SETTLE = 90_000;

// Type a query and wait until the grid has actually painted *its*
// results.
//
// The naive form — fill, then poll the cells — races a repaint that has
// no completion signal: GridCard keeps the previous result set on
// screen while a query is in flight (deliberately, so it doesn't flash
// empty on every keystroke), so a poll that runs early sees stale rows
// and cannot tell them from the answer. Asserting on the spinner has
// the opposite race, since `loading` flips both ways inside one tick.
//
// `data-shown-query` is the unambiguous signal: GridCard sets it to the
// query whose rows it just put up, so waiting for it to equal `q` means
// the grid is showing this search and not the last one. Callers assert
// on cell contents afterwards, with no polling needed.
export async function searchAndSettle(
  page: Page,
  q: string,
  opts: { grid?: Locator; timeout?: number } = {},
) {
  const grid = opts.grid ?? page.locator(".grid-wrap");
  await page.getByTestId("search-input").fill(q);
  await expect(grid).toHaveAttribute("data-shown-query", q, {
    timeout: opts.timeout ?? SEARCH_SETTLE,
  });
}
