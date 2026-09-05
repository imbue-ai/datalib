// AG Grid 36 renamed the DOM this file selects on. v35 split body rows
// horizontally into `.ag-center-cols-container` plus a pinned container per
// side; v36 has one element per vertical section
// (`.ag-grid-scrolling-rows`) with pinned cells held by sticky positioning.
// `.ag-body-viewport` likewise became `.ag-grid-viewport`, which is the
// element carrying `overflow: auto`.

import { expect, type Locator, type Page } from "@playwright/test";

// The DOM node for a row index. One definition, so the wait and the
// click can never drift onto different selectors.
const rowLocator = (page: Page, rowIndex: number): Locator =>
  page.locator(`.ag-grid-scrolling-rows [role="row"][row-index="${rowIndex}"]`);

// Ask the grid to put `uuid`'s row in the middle of the viewport, and
// report the index it lives at (null if no node carries that uuid).
const nudgeRowIntoView = (page: Page, uuid: string): Promise<number | null> =>
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

// Scroll a (possibly virtualized-away) row into view via the grid api
// the GridCard exposes on window, and return its row index once the DOM
// node for it actually exists.
//
// This is a race the suite could always lose and mostly didn't; it
// surfaced when the specs started running four at a time and rendering
// got slower relative to the scroll.
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
async function scrollRowIntoView(page: Page, uuid: string): Promise<number> {
  const rowIndex = await nudgeRowIntoView(page, uuid);
  expect(rowIndex, `node for uuid=${uuid} found in grid`).not.toBeNull();
  await expect
    .poll(
      async () => {
        await nudgeRowIntoView(page, uuid);
        return rowLocator(page, rowIndex as number).count();
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
// `scrollRowIntoView` returning means the row was rendered **then**. AG
// Grid can virtualize it away again before the action re-resolves the
// locator, and once the node is gone only another nudge brings it back
// — so retrying the action alone spins against a DOM that will never
// contain it, and retrying without a per-attempt timeout never gets to
// a second attempt at all. Playwright's default click timeout is the
// whole 30s test budget, so the first click consumed it waiting for a
// node that was already gone. Both halves are load-bearing; either one
// alone leaves the race in place.
async function actOnRowByUuid(
  page: Page,
  uuid: string,
  act: (row: Locator) => Promise<void>,
): Promise<void> {
  await expect(async () => {
    const rowIndex = await scrollRowIntoView(page, uuid);
    await act(rowLocator(page, rowIndex));
  }, `row uuid=${uuid} never took the action`).toPass({
    timeout: 15_000,
    intervals: [100, 250, 500],
  });
}

// Scroll a (possibly virtualized-away) row into view, then click it.
// Returns after the click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) => row.click({ timeout: 3_000 }));
}

// Select a row and confirm the grid agrees that it is selected.
export async function selectRowByUuid(page: Page, uuid: string): Promise<Locator> {
  const row = page.locator(`.ag-grid-scrolling-rows [role="row"][row-id="${uuid}"]`);
  // `aria-selected`, not the `ag-row-selected` class: the attribute is
  // AG Grid reporting the node's selection state, while the class is the
  // styling hook that follows from it. Asserting the semantic one means
  // a re-theme cannot break this and a half-applied render cannot pass
  // it.
  await expect(async () => {
    if ((await row.getAttribute("aria-selected")) !== "true") {
      await clickRowByUuid(page, uuid);
    }
    await expect(row).toHaveAttribute("aria-selected", "true", { timeout: 1_000 });
  }, `row ${uuid} never became selected`).toPass({
    timeout: 15_000,
    intervals: [100, 250, 500],
  });
  return row;
}

// Right-click a row located by uuid. Same virtualization dance as
// `clickRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) =>
    row.click({ button: "right", timeout: 3_000 }),
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
export const SEARCH_SETTLE = 90_000;

// Type a query and wait until the grid has actually painted *its*
// results.
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

/// Has the runner closed its record for the run in flight?
///
/// `finished_at` is written by the runner when the run ends, so it is a
/// fact rather than an inference — unlike `run.live`, which the endpoint
/// derives by probing the lock. A root that has never run reports no run
/// at all, which counts as closed: there is nothing in flight to be
/// wrong about.
async function runIsClosed(page: Page): Promise<boolean> {
  const dag = await (await page.request.get("/api/dag")).json();
  return !dag.run || dag.run.finished_at != null;
}

/// Wait for a row to finish a run newer than the one it was showing.
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
        const done = TERMINAL.test(last) && stamp !== before;
        // "Interrupted" is the one terminal status the UI INFERS rather
        // than reads: `stepStatus` reports it when a step says
        // `running` on a run with no `finished_at` while `GET /api/dag`
        // says no runner holds the lock — and that endpoint answers by
        // taking the lock itself, which its own comment calls "racy by
        // nature". So the instant a runner is taking or dropping the
        // root looks like a run that died, and a settle that believes it
        // returns "Interrupted" for a healthy `Sync everything`.
        if (done && last === "Interrupted" && !(await runIsClosed(page))) {
          return `${last} @ ${stamp} (run record still open)`;
        }
        return done ? "finished" : `${last} @ ${stamp}`;
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
