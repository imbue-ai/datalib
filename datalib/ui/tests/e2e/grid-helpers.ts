// AG Grid 36 renamed the DOM this file selects on, and the rename is not
// cosmetic: v35 split body rows horizontally into
// `.ag-center-cols-container` plus a pinned container per side, and v36
// dropped that split entirely — rows are one element per vertical
// section (`<ag-row-container name="scrolling">`, class
// `.ag-grid-scrolling-rows`) with pinned cells held in place by sticky
// positioning instead. `.ag-body-viewport` likewise became
// `.ag-grid-viewport`, which is the element carrying `overflow: auto`.
//
// Both old classes are simply absent from v36, so every selector using
// them matched nothing and 39 e2e tests failed while the grid itself
// rendered fine.

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
          .locator(`.ag-grid-scrolling-rows [role="row"][row-index="${rowIndex}"]`)
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

// Scroll a row into view and act on it, retrying the *pair*.
//
// `scrollRowIntoView` returning means the row was rendered **then**.
// AG Grid can virtualize it away again before the action re-resolves
// the locator, and once the node is gone only another nudge brings it
// back — so retrying the action alone would spin against a DOM that
// will never contain it. This is the same race the comment above
// describes, one step later: that fix put the nudge inside the poll,
// and left this gap between the poll and the click.
//
// The per-attempt timeout is the load-bearing part. Playwright's
// default is the whole 30s test timeout, so the first click consumed
// the entire budget waiting for a node that was already gone and the
// test died having never re-scrolled — which is exactly how this
// failed in CI (`yolink-plots`, webkit), while passing in isolation
// where nothing competes for the render.
async function actOnRowByUuid(
  page: Page,
  uuid: string,
  act: (row: Locator) => Promise<void>,
): Promise<void> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 4; attempt++) {
    const rowIndex = await scrollRowIntoView(page, uuid);
    try {
      await act(
        page.locator(
          `.ag-grid-scrolling-rows [role="row"][row-index="${rowIndex}"]`,
        ),
      );
      return;
    } catch (e) {
      lastError = e;
    }
  }
  throw lastError;
}

// Scroll a (possibly virtualized-away) row into view, then click it.
// Returns after the click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) => row.click({ timeout: 5_000 }));
}

// Right-click a row located by uuid. Same virtualization dance as
// `clickRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) =>
    row.click({ button: "right", timeout: 5_000 }),
  );
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
// One implementation, because three specs and two pull requests
// converged on this problem within a day of each other and each left a
// copy behind. #235 fixed a terminal-status set that listed three of
// the five states — in one of the two places that set existed. #236
// then fixed settling twice over, once per spec, arriving at two
// different answers: `onboarding-pdf` keyed on the row's stamp moving,
// `manager2-sync` waited for the runner to drop the data-root lock.
//
// Both were right about different halves, and neither is sufficient
// alone, so this is the union of them:
//
//   1. the row is terminal *and* reports an instant later than the one
//      it showed before the click. "Terminal" alone cannot tell a
//      finished run from the previous one — a second sync of an
//      already-succeeded row leaves "Succeeded" up until the job claims
//      it, so a status-only wait passes instantly against the old run.
//   2. the runner has let go of the data root. A terminal *row* is not
//      a finished *run*: the scheduler keeps walking the graph to
//      publish output versions, and those writes move `Last synced` on
//      rows a test then reads. `run.live` is the lock, so it is the
//      only signal that means nobody is writing.
//   3. the page is showing that. The grid refetches when the runner's
//      record moves, but that is debounced (300 ms, see
//      backend/http/src/watch.rs), so it can still be a beat behind the
//      backend right after the lock drops.

/// A Pipeline row, by the step id `getRowId` keys on.
export const pipelineRow = (page: Page, id: string) =>
  page.locator(`.ag-row[row-id="${id}"]`);

/// A row's status. The column paints an icon, so the state is the
/// icon's accessible name — the same word a person gets by hovering.
/// Null while the cell is mid-repaint or the row is virtualized away.
export async function statusOf(page: Page, id: string): Promise<string | null> {
  const el = pipelineRow(page, id).locator('[col-id="status"] [role="img"]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("aria-label");
}

/// The exact instant a row last ran, off the Last-synced cell's
/// `title`. Not the visible text, which reads "5 minutes ago" and
/// drifts on its own — comparing that across a sync would compare two
/// clocks rather than two records. Null for a row that has never run,
/// which renders "—" with no title to read.
export async function stampOf(page: Page, id: string): Promise<string | null> {
  const el = pipelineRow(page, id).locator('[col-id="lastSynced"] [title]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("title");
}

/// States a run will not move a step out of.
export const TERMINAL = /^(Succeeded|Up to date|Failed|Blocked|Interrupted)$/;

/// How long a row may take to settle: a real `datalib-dag` run over the
/// fixture corpus on a cold action cache.
export const ROW_SETTLE = 60_000;

/// Record every status a set of rows *passes through*, from now until
/// the page navigates.
///
/// Sampling on a timer — a `for(;;)` loop around `waitForTimeout(150)`,
/// which is what this replaces — can only see the states that happen to
/// be on screen when it looks, so a transition shorter than one
/// interval is invisible. That is not merely slow: a status that goes
/// *backwards* for less than a sample is exactly the bug
/// `manager2-sync`'s monotonicity check exists to catch, and the
/// sampler could miss it. Observing mutations instead makes the
/// sequence complete.
///
/// Watches `aria-label` because that is where the state lives (the
/// column paints icons), and `childList` because AG Grid rebuilds a
/// cell's DOM on `refreshCells` rather than mutating it in place.
///
/// The log lives in the page, so a navigation ends it — see
/// `settleRow`, which is the settle that does not remount.
export async function recordStatuses(page: Page, ids: readonly string[]) {
  await page.evaluate((ids: string[]) => {
    const w = window as unknown as {
      __statusLog?: Record<string, string[]>;
      __sampleStatuses?: () => void;
    };
    const log: Record<string, string[]> = {};
    w.__statusLog = log;
    const sample = () => {
      for (const id of ids) {
        const el = document.querySelector(
          `.ag-row[row-id="${CSS.escape(id)}"] [col-id="status"] [role="img"]`,
        );
        const s = el?.getAttribute("aria-label");
        const seen = (log[id] ??= []);
        if (s && label(seen[seen.length - 1]) !== s) {
          // The cell's tooltip carries *why* the status is what it is —
          // which upstream step a queued row is behind, how a run died.
          // Recording it costs nothing and is the difference between
          // "went backwards: [Queued, Running, Queued]" and knowing
          // which branch produced that third frame. The status is
          // everything up to the first " — ", so `label` splits it
          // back off for the comparison and for callers that only want
          // the word.
          const why = el?.closest("[title]")?.getAttribute("title") ?? "";
          seen.push(why.startsWith(`${s} — `) ? why : s);
        }
      }
    };
    /// The status word out of a recorded frame, which may carry its
    /// tooltip after an em dash.
    const label = (frame: string | undefined) => frame?.split(" — ")[0];
    sample();
    // Exposed so `statusLog` can take a reading of its own — see there.
    w.__sampleStatuses = sample;
    new MutationObserver(sample).observe(document.body, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["aria-label"],
    });
  }, ids as string[]);
}

/// The status word out of a frame `statusLog` returned. Frames carry
/// their tooltip after an em dash, so the whole frame is what you want
/// in a failure message and this is what you want to compare.
export function statusWord(frame: string): string {
  return frame.split(" — ")[0];
}

/// What `recordStatuses` has seen for one row, oldest first, ending
/// with what the row reads *now*.
///
/// The trailing sample is not belt-and-braces; without it this is
/// flaky, and was. A caller reaches here after `settleRow`, which
/// decides the row is terminal from its own reads — two Playwright
/// round-trips, on their own schedule. The observer's callback for that
/// same repaint is delivered separately, and nothing orders the two, so
/// the log could still end at "Running" while `settleRow` had already
/// returned "Succeeded". Seen once as
/// `expect(seen[seen.length - 1]).toBe("Succeeded")` receiving
/// "Running".
///
/// Sampling here closes it by construction: whatever the observer has
/// or has not delivered, the log ends with the state on screen at the
/// moment it was read. The dedupe in `sample` makes it a no-op when the
/// observer did get there first, which is the common case.
export async function statusLog(page: Page, id: string): Promise<string[]> {
  return page.evaluate((id: string) => {
    const w = window as unknown as {
      __statusLog?: Record<string, string[]>;
      __sampleStatuses?: () => void;
    };
    w.__sampleStatuses?.();
    return w.__statusLog?.[id] ?? [];
  }, id);
}

/// Wait for a row to finish a run newer than the one it was showing.
///
/// `before` is that row's stamp read *before* the click — see
/// `stampsBefore`, which takes the reading for a whole set at once so a
/// caller cannot forget one.
async function settleRowOnly(
  page: Page,
  id: string,
  before: string | null,
  timeout: number,
): Promise<string> {
  let last = "(no status)";
  await expect
    .poll(
      async () => {
        last = (await statusOf(page, id)) ?? "(no status)";
        const stamp = await stampOf(page, id);
        return TERMINAL.test(last) && stamp !== before ? "finished" : `${last} @ ${stamp}`;
      },
      {
        timeout,
        intervals: [200],
        message: `${id} never finished a run newer than ${before ?? "(never run)"}`,
      },
    )
    .toBe("finished");
  // The value the poll matched, never a fresh read: the row can be
  // claimed by the next job between the two, and the function would
  // then return "Queued" from a call whose contract is a terminal
  // status.
  return last;
}

/// Wait until no runner holds the data root, then remount so the page
/// is not a beat behind it.
///
/// `page.request` rather than the `request` fixture: it shares the
/// page's context, so it carries the same auth header and the same
/// baseURL — which matters now that config-mutating specs each have a
/// backend of their own.
export async function settleRunner(page: Page, timeout = ROW_SETTLE) {
  await expect
    .poll(
      async () => {
        const dag = await (await page.request.get("/api/dag")).json();
        return dag.run?.live === true;
      },
      { timeout, intervals: [200], message: "a runner still holds the data root" },
    )
    .toBe(false);
  await page.reload();
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

/// Every row's stamp, keyed by id — the reading a settle compares
/// against. Taken for the whole set before the click that starts a run.
export async function stampsBefore(
  page: Page,
  ids: readonly string[],
): Promise<Record<string, string | null>> {
  const out: Record<string, string | null> = {};
  for (const id of ids) out[id] = await stampOf(page, id);
  return out;
}

/// Settle every row a run was expected to reach, then wait out the run
/// itself. Returns each row's settled status, keyed by id.
///
/// The runner wait happens once at the end rather than per row: it is a
/// fact about the run, not about any one step, and remounting between
/// rows would only cost page loads.
export async function settleRows(
  page: Page,
  ids: readonly string[],
  before: Record<string, string | null>,
  timeout = ROW_SETTLE,
): Promise<Record<string, string>> {
  const out: Record<string, string> = {};
  for (const id of ids) out[id] = await settleRowOnly(page, id, before[id] ?? null, timeout);
  await settleRunner(page, timeout);
  return out;
}

/// `settleRows` for one row, without the remount.
///
/// For a caller holding a transition log recorded by `recordStatuses`:
/// the log lives in the page, so the `page.reload()` inside
/// `settleRunner` would throw it away before it could be read. Such a
/// caller has to settle the runner itself once it has the log.
export async function settleRow(
  page: Page,
  id: string,
  before: string | null,
  timeout = ROW_SETTLE,
): Promise<string> {
  return settleRowOnly(page, id, before, timeout);
}

/// One row's form of `settleRows`.
export async function settle(
  page: Page,
  id: string,
  before: string | null,
  timeout = ROW_SETTLE,
): Promise<string> {
  return (await settleRows(page, [id], { [id]: before }, timeout))[id];
}
