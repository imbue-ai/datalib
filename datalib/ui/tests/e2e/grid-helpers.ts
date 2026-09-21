// Every grid is a SlickGrid inside a card's shadow root (Playwright's
// locators pierce that). The search grid's rows carry `data-row` — the
// index the grid renders them at — and nothing naming the record, so a
// row is found by asking the card's grid api (`window.__fwGridApi`,
// see cards/GridCard.ce.vue) where a uuid's row is. The typed table
// viewer's rows (the Manage tree, the commit history) carry their key
// as `data-key`; those helpers are further down.

import { expect, type Locator, type Page } from "@playwright/test";

/// The search grid's rows, wherever it is on the page.
export const SEARCH_ROWS = ".grid-box .slick-row";
/// One of its column headers.
export const searchHeader = (page: Page, colId: string) =>
  page.locator(`.grid-box .slick-header-column[col-id="${colId}"]`);
/// The grid itself, for a "did it paint" check.
export const searchGrid = (page: Page) => page.locator(".grid-box .slickgrid-container");
/// The right-click menu the grid appends to <body>.
export const SEARCH_MENU = ".slick-context-menu";
/// One of its entries, by its text — the text beside the icon slot,
/// which reads as a bullet when the entry has no icon.
export const searchMenuItem = (page: Page, name: string | RegExp) =>
  page.locator(`${SEARCH_MENU} .slick-menu-content`).filter({ hasText: name });

/// The card's grid api on `window.__fwGridApi`, for `page.evaluate`.
export type GridApi = {
  rowIndexOf: (uuid: string) => number | null;
  uuidAt: (row: number) => string | null;
  rows: () => Record<string, unknown>[];
  filteredRows: () => Record<string, unknown>[];
  scrollToRow: (row: number) => void;
  scrollToColumn: (id: string) => void;
  isSelected: (uuid: string) => boolean;
  hiddenColumns: () => string[];
  showColumns: (ids: string[]) => void;
  groupBy: (ids: string[]) => void;
};

/// The uuid of the first row the grid has, whatever is at the top of
/// the viewport — a stable handle for a row that a scroll or a sort
/// would otherwise move out from under `.first()`.
export async function firstRowUuid(page: Page): Promise<string> {
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 10_000 });
  const uuid = await page.evaluate(
    () => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(0),
  );
  expect(uuid, "the grid must have a first row").toBeTruthy();
  return uuid!;
}

// The DOM node for a row index. One definition, so the wait and the
// click can never drift onto different selectors.
const rowLocator = (page: Page, rowIndex: number): Locator =>
  page.locator(`${SEARCH_ROWS}[data-row="${rowIndex}"]`);

// Ask the grid to put `uuid`'s row in view, and report the index it
// lives at (null if no row carries that uuid).
//
// `colId` nudges the horizontal axis too. The grid virtualizes both, so
// a caller that wants to read one particular cell has to name its
// column — otherwise the row is there and the cell it came for is not.
const nudgeRowIntoView = (
  page: Page,
  uuid: string,
  colId?: string,
): Promise<number | null> =>
  page.evaluate(
    ({ uuid, colId }) => {
      const a = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
      const row = a.rowIndexOf(uuid);
      if (row != null) a.scrollToRow(row);
      if (colId) a.scrollToColumn(colId);
      return row;
    },
    { uuid, colId },
  );

// Scroll a (possibly virtualized-away) row into view via the grid api
// the GridCard exposes on window, and return its row index once the DOM
// node for it actually exists.
//
// The scroll and the wait cannot be one step: the grid renders the
// newly-visible window on its own schedule, so the node at that index
// may not be in the DOM yet when `evaluate` returns, and if the
// viewport did not end up where the call asked (a re-layout, a grid
// that has just been resized), waiting alone never converges. So the
// nudge is inside the poll, and gets repeated until the row is there.
async function scrollRowIntoView(
  page: Page,
  uuid: string,
  colId?: string,
): Promise<number> {
  const rowIndex = await nudgeRowIntoView(page, uuid, colId);
  expect(rowIndex, `row for uuid=${uuid} found in grid`).not.toBeNull();
  await expect
    .poll(
      async () => {
        await nudgeRowIntoView(page, uuid, colId);
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
// `scrollRowIntoView` returning means the row was rendered **then**.
// The grid can virtualize it away again before the action re-resolves
// the locator, and once the node is gone only another nudge brings it
// back — so retrying the action alone spins against a DOM that will
// never contain it, and retrying without a per-attempt timeout never
// gets to a second attempt at all. Both halves are load-bearing.
export async function actOnRowByUuid<T>(
  page: Page,
  uuid: string,
  act: (row: Locator) => Promise<T>,
  colId?: string,
): Promise<T> {
  let out!: T;
  await expect(async () => {
    const rowIndex = await scrollRowIntoView(page, uuid, colId);
    out = await act(rowLocator(page, rowIndex));
  }, `row uuid=${uuid} never took the action`).toPass({
    timeout: 15_000,
    intervals: [100, 250, 500],
  });
  return out;
}

// Scroll a (possibly virtualized-away) row into view, then click it.
// Returns after the click; callers assert on the consequences.
export async function clickRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) => row.click({ timeout: 3_000 }));
}

// Select a row and confirm the grid agrees that it is selected — asked
// of the grid's own selection model, not read off a styling class.
export async function selectRowByUuid(page: Page, uuid: string): Promise<Locator> {
  const selected = () =>
    page.evaluate(
      (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
      uuid,
    );
  await expect(async () => {
    if (!(await selected())) await clickRowByUuid(page, uuid);
    await expect.poll(selected, { timeout: 1_000 }).toBe(true);
  }, `row ${uuid} never became selected`).toPass({
    timeout: 15_000,
    intervals: [100, 250, 500],
  });
  const rowIndex = await scrollRowIntoView(page, uuid);
  return rowLocator(page, rowIndex);
}

// Right-click a row located by uuid. Same virtualization dance as
// `clickRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) =>
    row.click({ button: "right", timeout: 3_000 }),
  );
  await expect(page.locator(SEARCH_MENU)).toBeVisible({ timeout: 5_000 });
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

// Assert that a grid actually *painted*, not merely mounted.
export async function expectGridPainted(
  grid: Locator,
  what: string,
  timeout = 10_000,
) {
  await expect(grid).toBeVisible({ timeout });
  await expect
    .poll(async () => (await grid.boundingBox())?.height ?? 0, {
      message: `${what}: the grid must have real height, not a collapsed box`,
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

// ── The typed table viewer's rows ─────────────────────────────────────

/// The rows of any `TableGrid` on the page — the Manage tree, the
/// commit history — scoped by a caller that has more than one open.
export const TABLE_ROWS = ".tg-grid .slick-row";
/// The right-click menu the grid appends to <body>, and its entries.
export const TABLE_MENU = ".slick-context-menu";
/// An entry by its text — the text beside the icon slot, which reads as
/// a bullet when the entry has no icon, so the whole item never matches
/// an anchored pattern.
export const menuEntry = (page: Page, entry: string | RegExp) =>
  page
    .locator(`${TABLE_MENU} .slick-menu-item`)
    .filter({ has: page.locator(".slick-menu-content").filter({ hasText: entry }) });
/// The class an entry carries when it cannot be taken.
export const MENU_DISABLED = /slick-menu-item-disabled/;
/// A row the grid has selected: its cells carry the class.
export const SELECTED_ROWS = `${TABLE_ROWS}:has(.slick-cell.selected)`;

/// Manager2 with the config editor (`.m2-editor`) open beside the
/// sources card. `/data_sources` opens the sources card alone, which is
/// what a person gets; a spec that reads or writes `config.toml`
/// through the editor asks for both cards by their stack.
export const MANAGE_WITH_CONFIG = "/sourcesView():1.6/configView()";

/// A Pipeline row, by the key its record carries. A step under a
/// group has a row only while the group is open — see `expandGroup`.
export const pipelineRow = (page: Page, id: string) =>
  page.locator(`${TABLE_ROWS}[data-key="${id}"]`);

/// A group's row. Keyed `group:<id>` because an applet may share the
/// group's id (`unified_index` does) and both are rows.
export const groupRow = (page: Page, id: string) => pipelineRow(page, `group:${id}`);

/// Open a tree row so the rows under it exist. Idempotent, and the
/// grid remembers what was opened across a remount — which `settle`
/// does — so one call per group per test is enough.
///
/// The click and the check are retried as a pair. A save remounts the
/// table, and a click that lands on the chevron of a row the grid is
/// about to replace opens nothing; the row that takes its place is
/// folded again, and a check on its own would wait on it forever.
export async function expandRow(row: Locator, what: string): Promise<void> {
  await expect(row, `${what} should have a row`).toBeVisible();
  await expect(async () => {
    const closed = row.locator(".slick-tree-toggle.collapsed");
    if ((await closed.count()) > 0) await closed.click({ timeout: 1_000 });
    await expect(row.locator(".slick-tree-toggle.expanded")).toBeVisible({ timeout: 1_000 });
  }, `${what} never opened`).toPass({ timeout: 15_000, intervals: [100, 250, 500] });
}

export async function expandGroup(page: Page, id: string): Promise<void> {
  await expandRow(groupRow(page, id), `group ${id}`);
}

/// One entry of a Manage row's right-click menu, opened on its Status
/// cell. The row's menu is where its actions live; Sync is the one
/// button left.
export function rowMenuEntry(page: Page, row: Locator, entry: string | RegExp) {
  return {
    open: async () => {
      await row.locator('[col-id="status"]').click({ button: "right" });
      const option = menuEntry(page, entry);
      await expect(option).toBeVisible({ timeout: 2_000 });
      return option;
    },
  };
}

/// Pick a Manage row's menu entry and wait for what it does, retrying the
/// pair. A save remounts the table, and a menu opened on a row the grid
/// is about to replace closes with it, so one attempt is a race. `effect`
/// already visible means an earlier attempt landed, so nothing is picked
/// twice.
export async function pickRowMenu(
  page: Page,
  row: Locator,
  entry: string | RegExp,
  effect: Locator,
): Promise<void> {
  await expect(async () => {
    if (await effect.isVisible()) return;
    await page.keyboard.press("Escape");
    const option = await rowMenuEntry(page, row, entry).open();
    await option.click({ timeout: 2_000 });
    await expect(effect).toBeVisible({ timeout: 2_000 });
  }, `${String(entry)} never took`).toPass({
    timeout: 15_000,
    intervals: [250, 500, 1_000],
  });
}

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
  const el = pipelineRow(page, id).locator('[col-id="last_synced"] [title]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("title");
}

/// States a run will not move a step out of.
export const TERMINAL = /^(Succeeded|Up to date|Failed|Blocked|Interrupted|Stopped)$/;


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
    // The Sources grid lives in a card's shadow root, which neither
    // `document.querySelector` nor an observer on `document.body` can
    // see into: query every open shadow root, and observe them too.
    const roots = (): (Document | ShadowRoot)[] => {
      const out: (Document | ShadowRoot)[] = [document];
      for (const el of document.querySelectorAll("*")) {
        if (el.shadowRoot) out.push(el.shadowRoot);
      }
      return out;
    };
    const deepQuery = (sel: string): Element | null => {
      for (const r of roots()) {
        const el = r.querySelector(sel);
        if (el) return el;
      }
      return null;
    };
    const sample = () => {
      for (const id of ids) {
        const el = deepQuery(
          `.slick-row[data-key="${CSS.escape(id)}"] [col-id="status"] [role="img"]`,
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
    const observer = new MutationObserver(sample);
    for (const r of roots()) {
      observer.observe(r === document ? document.body : r, {
        subtree: true,
        childList: true,
        attributes: true,
        attributeFilter: ["aria-label"],
      });
    }
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
/// What the backend says about the run right now, for a failure that
/// would otherwise report only a status word.
///
/// The three fields that decide `Interrupted` are `run.live` (the
/// backend's lock probe), `run.finished_at`, and the step's
/// `current_state`; printing all three separates "a run really died"
/// from "the lock probe lost its race" without opening a trace.
async function dumpRunnerState(page: Page, id: string, why: string): Promise<void> {
  try {
    const dag = await (await page.request.get("/api/dag")).json();
    const step = (dag.steps ?? []).find((s: { id: string }) => s.id === id);
    console.warn(
      `[e2e] ${why} for ${id}: run=${JSON.stringify(dag.run ?? null)} ` +
        `current_state=${JSON.stringify(step?.current_state ?? null)} ` +
        `last_run=${JSON.stringify(step?.last_run ?? null)}`,
    );
  } catch (e) {
    console.warn(`[e2e] ${why} for ${id}: could not read /api/dag: ${e}`);
  }
}

async function runIsClosed(page: Page): Promise<boolean> {
  const dag = await (await page.request.get("/api/dag")).json();
  // `ok: false` is the endpoint failing to read the root at that
  // instant — its record mid-write, say — and it then carries no run
  // at all. That is not a closed run; it is no answer. Reading it as
  // closed is how a healthy `Sync everything` once settled as
  // "Interrupted" on CI, with the dump showing every field null.
  if (dag.ok === false) return false;
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
  try {
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
          if (done && last === "Interrupted") {
            if (!(await runIsClosed(page))) return `${last} @ ${stamp} (run record still open)`;
            // The run has closed, so the next repaint says what really
            // happened; a verdict inferred before the close is not it.
            return `${last} @ ${stamp} (waiting for the grid to catch up with the closed run)`;
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
  } finally {
    // The poll never returns on "Interrupted", so ending on it means
    // the poll timed out on a row that stayed that way: leave behind
    // the evidence that says whether a run really died — Playwright
    // puts test stdout in the report and in bazel's test log.
    if (last === "Interrupted") {
      await dumpRunnerState(page, id, "settle timed out on Interrupted");
    }
  }
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
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
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
