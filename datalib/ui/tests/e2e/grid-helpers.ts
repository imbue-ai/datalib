import { expect, type Locator, type Page } from "@playwright/test";

// Scroll a (possibly virtualized-away) row into view via the grid api
// the GridCard exposes on window, then click it. Returns after the
// click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  const rowIndex = await page.evaluate(
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
  expect(rowIndex, `node for uuid=${uuid} found in grid`).not.toBeNull();
  await page
    .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
    .click();
}

// Right-click a row located by uuid. Same virtualization dance as
// `clickRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  const rowIndex = await page.evaluate(
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
  expect(rowIndex, `node for uuid=${uuid} found in grid`).not.toBeNull();
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

// ── The Pipeline table's rows ────────────────────────────────────────
//
// Shared because there were two copies of all of this — one in
// `manager2-sync.spec.ts` and one in `onboarding-pdf.spec.ts` — and
// they had already drifted: #235 fixed a terminal-status set that
// listed three of the five statuses, in one of the two places it
// existed. One vocabulary, one settle rule.

/// A Pipeline row, by the step id `getRowId` keys on.
export const pipelineRow = (page: Page, id: string) =>
  page.locator(`.ag-row[row-id="${id}"]`);

/// The status cell, which carries the state and the run it belongs to.
const statusCell = (page: Page, id: string) =>
  pipelineRow(page, id).locator('[col-id="status"] .m2-status');

/// The runner's own word for a row's state (`skipped_up_to_date`), not
/// the display label ("Up to date"). Null while the cell is mid-repaint
/// or the row is virtualized away.
export async function stateOf(page: Page, id: string): Promise<string | null> {
  const el = statusCell(page, id);
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("data-status");
}

/// The instant the row's *currently painted* status describes — the
/// same value the Last synced column shows. This is a run's identity:
/// two runs of one step produce two different stamps, where the status
/// word produces the same string every time.
export async function stateAtOf(page: Page, id: string): Promise<string | null> {
  const el = statusCell(page, id);
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("data-status-at");
}

/// States a run will not move a step out of. The runner's vocabulary,
/// in one place.
export const TERMINAL_STATES = new Set([
  "succeeded",
  "skipped_up_to_date",
  "failed",
  "blocked",
  "interrupted",
]);

/// How long a row may take to settle. A real `datalib-dag` run over the
/// fixture corpus, on a cold action cache, under `--runs_per_test=N`.
export const ROW_SETTLE = 60_000;

/// Wait for a row to reach a terminal state, and return it.
///
/// `after` is what makes this honest. Without it the only question
/// askable is "is this row terminal?", and immediately after clicking
/// Sync the answer is *yes* — from the previous run, whose status is
/// still painted because the click has not yet produced a new one. The
/// poll is satisfied instantly, the caller proceeds, and the run it
/// started is still going: assertions then race it. That is one bug
/// with two faces — `onboarding-pdf` read a rendered_md row that was
/// still "Running", and `manager2-sync` compared a Last synced stamp
/// that the real run rewrote five seconds later.
///
/// Passing the stamp read *before* the click turns the question into
/// "is this row terminal in a run later than the one I already saw",
/// which no stale frame can answer yes to. Prefer `syncAndSettle`,
/// which cannot forget to take that reading.
///
/// Returns the state the poll actually matched rather than re-reading
/// the cell: a second, unsynchronized read can land after the row has
/// moved on again, which is how the old helper returned "Queued" from
/// a function whose poll had just proven the row terminal.
export async function settleRow(
  page: Page,
  id: string,
  opts: { after?: string | null; timeout?: number } = {},
): Promise<string> {
  let settled: string | null = null;
  await expect
    .poll(
      async () => {
        const state = await stateOf(page, id);
        if (!state || !TERMINAL_STATES.has(state)) return null;
        // Same stamp as before the click ⇒ this is the previous run's
        // status still on screen, not an outcome for this one.
        if (opts.after !== undefined && (await stateAtOf(page, id)) === opts.after) {
          return null;
        }
        settled = state;
        return state;
      },
      {
        timeout: opts.timeout ?? ROW_SETTLE,
        intervals: [200],
        message:
          `${id} never reached a terminal state` +
          (opts.after !== undefined ? ` in a run after ${opts.after ?? "(never run)"}` : ""),
      },
    )
    .not.toBeNull();
  return settled!;
}

/// Press a source row's Sync button and wait for the run it starts to
/// reach every row named.
///
/// Takes the "before" reading itself, between locating the button and
/// pressing it, so no caller can start a run and then ask a question
/// that its own previous run already answers. Returns each row's
/// settled state, keyed by id.
export async function syncAndSettle(
  page: Page,
  seedId: string,
  watch: string[] = [seedId],
  opts: { timeout?: number } = {},
): Promise<Record<string, string>> {
  const before: Record<string, string | null> = {};
  for (const id of watch) before[id] = await stateAtOf(page, id);
  await pipelineRow(page, seedId).getByRole("button", { name: "Sync now" }).click();
  const settled: Record<string, string> = {};
  for (const id of watch) {
    settled[id] = await settleRow(page, id, {
      after: before[id],
      timeout: opts.timeout,
    });
  }
  return settled;
}
