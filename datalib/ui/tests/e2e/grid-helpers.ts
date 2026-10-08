// Every grid is a SlickGrid inside a card's shadow root (Playwright's
// locators pierce that). The search grid's rows carry `data-row` — the
// index the grid renders them at — and nothing naming the record, so a
// row is found by asking the card's grid api (`window.__fwGridApi`,
// see cards/GridCard.ce.vue) where a uuid's row is. The grid loads a
// search a page at a time, so a row further down is sought first. The typed table
// viewer's rows (the Manage tree, the commit history) carry their key
// as `data-key`; those helpers are further down. Before writing a spec,
// read docs/dev/testing.md §"Writing a spec that does not flake".

import { expect, type APIRequestContext, type Locator, type Page } from "@playwright/test";

/// The search grid's rows, wherever it is on the page.
// The containers layout: the tabs down the side, and the cards the
// selected tab shows (every card in it, however deep). A tab shown once
// stays mounted, hidden and marked ct-hidden-pane; its cards do not
// count, nor does a card that is the hidden tab itself. (Not
// `:visible`: a card that draws nothing is zero-high.)
export const SHOWN_CARDS = ".ct-main .ct-card:not(.ct-hidden-pane, .ct-hidden-pane .ct-card)";
export const shownCards = (page: Page) => page.locator(SHOWN_CARDS);
export const tabLabels = (page: Page) => page.locator(".ct-tab .ct-tab-label");
// The shown card whose source contains `source`.
export const cardOf = (page: Page, source: string) =>
  page.locator(`${SHOWN_CARDS}[data-card-source*=${JSON.stringify(source)}]`);
// A card's title, in the header it has inside a container outside a
// solidified one.
export const cardTitle = (card: Locator) => card.locator(".ct-card-title");
// The shown tab's name. A card that fills its tab has no header; its
// title names the tab.
export const shownTabName = (page: Page) => page.locator(".ct-tab.is-selected .ct-tab-label");

export const SEARCH_ROWS = ".grid-box .slick-row";

/// The search grid on its default query, documents only. `/` opens on
/// the Dashboard card now, so a spec about the grid goes here.
export const GRID = "/gridView()";

/// The search grid with its query cleared: every row, the messages
/// inside a document included. `GRID` opens on documents only.
export const EVERY_ROW = "/gridView()::q%3D";

/// The Manage header's sync button, whichever way it faces: Sync
/// everything, or Stop everything while anything syncs.
export const syncAllButton = (page: Page) =>
  page.getByRole("button", { name: /^(Sync|Stop|Stopping) everything$/ });
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
  /// Load pages until the row is held; its index, or null.
  seek: (uuid: string) => Promise<number | null>;
  /// What a header dropped on the search bar does.
  dropOnSearch: (colId: string) => void;
  /// A search, a page, or a group's page on its way.
  busy: () => boolean;
  uuidAt: (row: number) => string | null;
  rows: () => Record<string, unknown>[];
  scrollToRow: (row: number) => void;
  scrollToColumn: (id: string) => void;
  isSelected: (uuid: string) => boolean;
  /// The record under the active cell, where the arrow keys start.
  activeUuid: () => string | null;
  hiddenColumns: () => string[];
  showColumns: (ids: string[]) => void;
  groupBy: (ids: string[]) => void;
};

/// Wait until nothing the grid asked for is still on its way.
export async function gridSettled(page: Page) {
  await expect
    .poll(
      () => page.evaluate(() => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.busy()),
      {
        message: "the grid never finished loading",
      },
    )
    .toBe(false);
}

/// The uuid of the first row the grid has, whatever is at the top of
/// the viewport — a stable handle for a row that a scroll or a sort
/// would otherwise move out from under `.first()`.
export async function firstRowUuid(page: Page): Promise<string> {
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 10_000 });
  const uuid = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(0),
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
const nudgeRowIntoView = (page: Page, uuid: string, colId?: string): Promise<number | null> =>
  page.evaluate(
    async ({ uuid, colId }) => {
      const a = (window as unknown as { __fwGridApi: GridApi }).__fwGridApi;
      const row = a.rowIndexOf(uuid) ?? (await a.seek(uuid));
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
async function scrollRowIntoView(page: Page, uuid: string, colId?: string): Promise<number> {
  // The index is read afresh on every try: rows loading above this one,
  // as they do when the grid is scrolled towards its older end, move it.
  let rowIndex: number | null = null;
  await expect
    .poll(
      async () => {
        rowIndex = await nudgeRowIntoView(page, uuid, colId);
        return rowIndex == null ? -1 : rowLocator(page, rowIndex).count();
      },
      {
        timeout: 15_000,
        intervals: [100, 250, 250, 500],
        message: `row uuid=${uuid} was not found, or never rendered after being scrolled to`,
      },
    )
    .toBeGreaterThan(0);
  return rowIndex as unknown as number;
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

// Where a click on a row lands: near its left end, not its middle. The
// columns keep their widths, so a row can be wider than the grid, and
// its middle scrolled out of sight — Playwright then finds whatever is
// drawn there (the "add a card" button, in one CI run), scrolls the
// grid sideways to reach the row, and the click lands on a row the
// grid has redrawn under it.
const ROW_CLICK_POINT = { x: 40, y: 10 };

// Select a row and confirm the grid agrees that it is selected — asked
// of the grid's own selection model, not read off a styling class.
//
// Every attempt asks the grid first and clicks only if it says no. A
// click whose mouse events landed can still throw: on a loaded runner
// WebKit has answered the click three seconds late. A second click on
// the row would then scroll the Columns row back to it — undoing the
// reveal of the document column the first click opened — and select
// nothing new. So a thrown click is not retried as a pair; the next
// attempt starts from the grid's answer.
export async function selectRowByUuid(page: Page, uuid: string): Promise<Locator> {
  const selected = () =>
    page.evaluate(
      (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
      uuid,
    );
  await expect(async () => {
    if (await selected()) return;
    const rowIndex = await scrollRowIntoView(page, uuid);
    await rowLocator(page, rowIndex).click({ position: ROW_CLICK_POINT, timeout: 3_000 });
    await expect.poll(selected, { timeout: 1_000 }).toBe(true);
  }, `row ${uuid} never became selected`).toPass({
    timeout: 15_000,
    intervals: [100, 250, 500],
  });
  const rowIndex = await scrollRowIntoView(page, uuid);
  return rowLocator(page, rowIndex);
}

// Right-click a row located by uuid. Same virtualization dance as
// `selectRowByUuid` — a row scrolled out of the viewport has no DOM
// node to dispatch at — but opens the context menu instead of
// selecting.
export async function contextMenuRowByUuid(page: Page, uuid: string) {
  await actOnRowByUuid(page, uuid, (row) =>
    row.click({ button: "right", position: ROW_CLICK_POINT, timeout: 3_000 }),
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
    page.evaluate(() => (window as unknown as { __copied?: string }).__copied ?? null);
}

// Assert that a grid actually *painted*, not merely mounted.
export async function expectGridPainted(grid: Locator, what: string, timeout = 10_000) {
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
/// A grid with pinned columns draws each row in two halves; this is the
/// half that scrolls, which holds every cell but the pinned Name
/// (`nameCell`), so a row is still one element.
export const TABLE_ROWS = ".tg-grid .slick-row:not([data-pinned])";
/// A row's Name cell, in whichever half of the row the grid draws it.
export const nameCell = (page: Page, key: string) =>
  page.locator(`.tg-grid .slick-row[data-key="${key}"] [col-id="name"]`);
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

/// The config as the server holds it, for a spec to put back when it is
/// done. Read from the API rather than the editor: the editor fills in
/// after the card paints, and a read that beats it snapshots nothing —
/// the spec then writes a config with no applet, and every spec after it
/// in the file opens on the config-error screen.
export async function savedConfig(request: APIRequestContext): Promise<string> {
  const { text } = (await (await request.get("/api/config")).json()) as { text: string };
  expect(text, "the server should hold a config to put back").toContain("[[applets]]");
  return text;
}

/// The config editor (`.m2-editor`) open beside the sources card. `/data_sources` opens the sources card alone, which is
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
export async function expandRow(page: Page, key: string, what: string): Promise<void> {
  const name = nameCell(page, key);
  await expect(name, `${what} should have a row`).toBeVisible();
  await expect(async () => {
    const closed = name.locator(".slick-tree-toggle.collapsed");
    if ((await closed.count()) > 0) await closed.click({ timeout: 1_000 });
    await expect(name.locator(".slick-tree-toggle.expanded")).toBeVisible({ timeout: 1_000 });
  }, `${what} never opened`).toPass({ timeout: 15_000, intervals: [100, 250, 500] });
}

export async function expandGroup(page: Page, id: string): Promise<void> {
  await expandRow(page, `group:${id}`, `group ${id}`);
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
  await expect(
    async () => {
      if (await effect.isVisible()) return;
      await page.keyboard.press("Escape");
      const option = await rowMenuEntry(page, row, entry).open();
      await option.click({ timeout: 2_000 });
      await expect(effect).toBeVisible({ timeout: 2_000 });
    },
    `${String(entry)} never took`,
  ).toPass({
    timeout: 15_000,
    intervals: [250, 500, 1_000],
  });
}

/// One drawing of a Pipeline row: every cell a spec reads, taken in one
/// pass over the DOM so no two values come from different paints. The
/// server sends a row with its config and the loop's record already
/// joined, so in a drawn row a null means the value is really absent.
export type RowReading = {
  /// The status icon's accessible name — the word a person gets by
  /// hovering.
  status: string;
  /// The exact instants, off the stamps' `title`. Not the visible
  /// "5 minutes ago", which drifts on its own. `lastSynced` is the stamp
  /// beside the Last update glyph, which on a step is its last sync.
  /// Null: the step never ran (or never succeeded), or its column is
  /// hidden — `lastSuccessOf` shows it first.
  lastSynced: string | null;
  lastSuccess: string | null;
  /// The Bytes label over the sparkline, as drawn. Null: nothing on disk.
  disk: string | null;
  /// The Queue and ETA cells as drawn — a figure, or a word such as
  /// "stalled"; "" when blank.
  queue: string;
  eta: string;
};

/// The time beside the Last update glyph; absent on a row that never ran.
export const LAST_UPDATE_AT = '[col-id="status"] .tg-status-at';

/// How long a row may take to be drawn: a remount fetches the rows after
/// the shell has painted.
export const ROW_DRAWN = 15_000;

/// A row as drawn right now, or null while it is not drawn: its group is
/// closed, the page is remounting, or the grid is repainting it. Only for
/// a poll that treats the null as "not yet"; everything else reads
/// through `readRow`, which waits it out.
export async function sampleRow(page: Page, id: string): Promise<RowReading | null> {
  const [reading] = await pipelineRow(page, id).evaluateAll(
    (rows, at) =>
      rows.slice(0, 1).map((row) => {
        const status = row
          .querySelector('[col-id="status"] [role="img"]')
          ?.getAttribute("aria-label");
        if (!status) return null;
        const quantity = (el: Element, col: string) => {
          const text = el.querySelector(`[col-id="${col}"] .tg-quantity`)?.textContent?.trim();
          return !text || text === "—" ? "" : text;
        };
        const stamp = (col: string) =>
          row.querySelector(`[col-id="${col}"] [title]`)?.getAttribute("title") ?? null;
        return {
          status,
          lastSynced: row.querySelector(at)?.getAttribute("title") ?? null,
          lastSuccess: stamp("last_success"),
          disk: row.querySelector('[col-id="disk"] .tg-plot-value')?.textContent?.trim() ?? null,
          queue: quantity(row, "queue"),
          eta: quantity(row, "eta"),
        };
      }),
    LAST_UPDATE_AT,
  );
  return reading ?? null;
}

/// A row once it is drawn. Returns the reading the wait matched, never a
/// fresh read, which could land on the next repaint.
export async function readRow(page: Page, id: string, timeout = ROW_DRAWN): Promise<RowReading> {
  let reading: RowReading | null = null;
  await expect
    .poll(async () => (reading = await sampleRow(page, id)) !== null, {
      timeout,
      intervals: [100, 200],
      message: `${id} was never drawn: is its group open?`,
    })
    .toBe(true);
  return reading!;
}

export async function statusOf(page: Page, id: string): Promise<string> {
  return (await readRow(page, id)).status;
}

/// Null only for a row that has never run.
export async function stampOf(page: Page, id: string): Promise<string | null> {
  return (await readRow(page, id)).lastSynced;
}

/// A column the Sources card hides until asked for, shown through the
/// header's right-click picker.
export async function showColumn(page: Page, field: string) {
  const header = (col: string) => page.locator(`.tg-grid .slick-header-column[col-id="${col}"]`);
  if ((await header(field).count()) > 0) return;
  await header("status").click({ button: "right" });
  const picker = page.locator(".slick-column-picker");
  await picker.locator("label", { has: page.locator(`input[data-columnid="${field}"]`) }).click();
  await picker.locator("button.close").click();
  await expect(header(field)).toHaveCount(1);
}

/// Null only for a row that has never succeeded.
export async function lastSuccessOf(page: Page, id: string): Promise<string | null> {
  await showColumn(page, "last_success");
  return (await readRow(page, id)).lastSuccess;
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
        // Status and stamp from one paint: read apart, "Stopped" from
        // before a sync beside the stamp that sync just wrote reads as
        // the sync having stopped.
        const reading = await sampleRow(page, id);
        if (!reading) return "(not drawn)";
        last = reading.status;
        const stamp = reading.lastSynced;
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
  // wanted by the next request between the two, and the function would
  // then return "Queued" from a call whose contract is a terminal
  // status.
  return last;
}

/// Wait until no sync is running — the server's loop is idle, or, before
/// it has the lock, no `datalib-dag` holds it — then remount so the page
/// is not a beat behind it.
export async function settleRunner(page: Page, timeout = ROW_SETTLE) {
  await expect
    .poll(
      async () => {
        const dag = await (await page.request.get("/api/dag")).json();
        return dag.run?.live === true;
      },
      { timeout, intervals: [200], message: "a sync is still running" },
    )
    .toBe(false);
  await page.reload();
  await expect(syncAllButton(page)).toBeVisible();
}

/// Resolves once the wall clock is in a later second than when it was
/// called. A stamp is to the second, and the server's loop takes a sync
/// on the moment it is asked: without this, a run started straight after
/// another can carry the same stamp, and a settle that waits for the
/// stamp to move waits for ever. Every stamp read before calling this is
/// of a run already over, so a run started after it stamps later.
export async function untilTheSecondTurns() {
  const now = Math.floor(Date.now() / 1000);
  await expect.poll(() => Math.floor(Date.now() / 1000), { intervals: [50] }).toBeGreaterThan(now);
}

/// Every row's stamp, keyed by id — the reading a settle compares
/// against. Taken for the whole set before the click that starts a run,
/// which it holds until a run started then would stamp later.
export async function stampsBefore(
  page: Page,
  ids: readonly string[],
): Promise<Record<string, string | null>> {
  const out: Record<string, string | null> = {};
  for (const id of ids) out[id] = await stampOf(page, id);
  await untilTheSecondTurns();
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
  for (const id of ids) await readRow(page, id, timeout);
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

/** A document card's rendered body. It is drawn inside the card's
 *  own frame (`src/cards/docFrame.ts`), so a page-level locator
 *  does not reach it. `scope` narrows to one card when several are open. */
export function docBody(scope: Page | Locator): Locator {
  return scope.frameLocator("iframe.doc-frame").locator("body.chat-body");
}

/** `selector` in whichever document frame holds it, waiting until one
 *  does. One locator cannot reach across frames, and each document card
 *  has its own frame. */
export async function inDocFrame(page: Page, selector: string, timeout = 10_000): Promise<Locator> {
  let hit: Locator | null = null;
  await expect
    .poll(
      async () => {
        for (const f of page.frames()) {
          if (f === page.mainFrame()) continue;
          const loc = f.locator(selector);
          if ((await loc.count().catch(() => 0)) > 0) {
            hit = loc;
            return true;
          }
        }
        return false;
      },
      { timeout, message: `no document frame holds ${selector}` },
    )
    .toBe(true);
  return hit!;
}
