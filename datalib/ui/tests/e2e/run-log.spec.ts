// The run-log card: opened on the server launch serving the page, it
// shows that launch's lines in a column beside Manage, a
// right-click on a cell narrows the query to that cell's value (and
// clears it again), the bar above the grid groups the lines by a
// column, and a selected line opens in full in the next column.
//
// The grid is built straight on the vanilla SlickGrid bundle, like the
// cards' grids; this is the one place its menu, grouping bar and query
// round-trip are exercised end to end.

import { test, expect, type Locator, type Page } from "@playwright/test";
import { menuEntry } from "./grid-helpers";

// The commit playwright.config.ts handed the backends. Node's globals
// are not in this tsconfig, as in api-token.spec.ts.
declare const process: { env: Record<string, string | undefined> };
const GIT_HASH = process.env.DATALIB_GIT_HASH;

const ROWS = ".rl-grid .slick-row:not(.slick-group)";

/// A value as the query bar writes it into a `key:value` token, the way
/// `src/grid/query.ts` does — spelled out here because a spec runs
/// outside the app's module graph.
const quoted = (v: string) =>
  /[\s:"]/.test(v) || v === "" || v.startsWith("-")
    ? `"${v.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`
    : v;

/// The status bar's "Logs" opens the log as the column after the
/// Manage card; picking this server's launch retitles it. The lines
/// already shown stay until the launch's replace them, so this waits
/// for that load to finish before anything reads a row.
async function openServerLog(page: Page) {
  await page.goto("/data_sources");
  await page.locator(".cards-statusbar").getByRole("button", { name: "Logs" }).click();
  const dialog = page.locator(".miller-col").filter({ has: page.locator(".rl-panel") });
  await expect(dialog).toBeVisible();
  const scope = dialog.getByLabel("Which run or launch");
  const mine = scope.locator("option", { hasText: /this server$/ });
  await expect(mine).toHaveCount(1);
  const value = (await mine.getAttribute("value"))!;
  const launch = value.replace(/^launch:/, "");
  const loaded = page.waitForResponse(
    (r) => r.url().includes("/api/log?") && r.url().includes(`process=${launch}`),
  );
  await scope.selectOption(value);
  await loaded;
  await expect(dialog.locator(".rl-panel")).toHaveAttribute("aria-busy", "false");
  await expect(dialog.locator(".miller-col-title")).toHaveText("Server log");
  await expect(dialog.locator(ROWS).first()).toBeVisible({ timeout: 10_000 });
  return dialog;
}

const lineCount = (page: Page) =>
  page
    .locator(".rl-count")
    .evaluate((el) => Number(/(\d+) line/.exec(el.textContent ?? "")?.[1] ?? NaN));

/// Right-clicks `target` once the grid has heard any scroll that
/// brings it into view. The Message cell sits partly past the grid's
/// right edge, and a click scrolls it in first; the scroll event is
/// delivered at the next frame, after the press has opened the menu, and
/// the grid's context menu closes on any scroll of the grid. Scroll
/// events are dispatched before a frame's animation callbacks, so one
/// frame is enough. Retried until `entry` shows, because a new line can
/// re-render the row and detach the cell before the press.
async function rightClick(target: Locator, entry: Locator) {
  await expect(async () => {
    await target.scrollIntoViewIfNeeded({ timeout: 1_000 });
    await target.evaluate(() => new Promise<void>((done) => requestAnimationFrame(() => done())));
    await target.click({ button: "right", timeout: 1_000 });
    await expect(entry).toBeVisible({ timeout: 1_000 });
  }).toPass();
}

test("a cell's right-click keeps only its value, and the query clears again", async ({ page }) => {
  const dialog = await openServerLog(page);
  // Opened on this server's launch — a process, picked like a run.
  const scope = dialog.getByLabel("Which run or launch");
  await expect(scope).toHaveValue(/^launch:/);
  await expect(scope.locator("option:checked")).toHaveText(/this server$/);
  const query = dialog.locator(".rl-search");
  await expect(query).toHaveValue("min_level:info");
  const all = await lineCount(page);
  expect(all).toBeGreaterThan(1);

  // The menu names the value under the click. Message is the column it
  // is opened on because `msg:` matches a line's message exactly, so
  // keeping only the first line's own message narrows to a set this
  // test can predict without knowing what the server logged. Not
  // trimmed, for the same reason: the token has to carry the value the
  // cell holds.
  const msgCell = dialog.locator(ROWS).first().locator('.slick-cell[col-id="msg"]');
  const msg = (await msgCell.textContent()) ?? "";
  expect(msg.trim(), "the first line should have a message").not.toBe("");
  // One right-click is enough: the panel holds the tail back while a
  // button is down on the grid, so the row is not re-rendered under it.
  const keepOnly = menuEntry(page, `Keep only Message=${msg}`);
  await rightClick(msgCell, keepOnly);
  await expect(menuEntry(page, `Exclude all Message=${msg}`)).toBeVisible();
  await keepOnly.click();

  await expect(query).toHaveValue(`min_level:info msg:${quoted(msg)}`);
  // A reload empties the count before it refills, so "fewer than all"
  // alone is met mid-way; wait for the narrowed lines to be there.
  await expect
    .poll(async () => {
      const n = await lineCount(page);
      return n > 0 && n < all;
    })
    .toBe(true);
  await expect
    .poll(async () => {
      const msgs = await dialog.locator(`${ROWS} .slick-cell[col-id="msg"]`).allTextContents();
      return [...new Set(msgs)];
    })
    .toEqual([msg]);

  const clear = menuEntry(page, "Clear the query");
  await rightClick(dialog.locator(ROWS).first().locator('.slick-cell[col-id="msg"]'), clear);
  await clear.click();
  await expect(query).toHaveValue("");
  // With no query at all, every line this launch wrote.
  await expect.poll(() => lineCount(page)).toBeGreaterThanOrEqual(all);

  // The level picker writes its word into the query, where it can be
  // read back, edited or cleared like anything typed.
  const level = dialog.getByLabel("Lowest level to show");
  await expect(level).toHaveValue("trace");
  await level.selectOption("warn");
  await expect(query).toHaveValue("min_level:warn");
  await level.selectOption("trace");
  await expect(query).toHaveValue("");
});

// The log opens on the seven columns a reader wants on every line. The
// other five are hidden rather than gone, and the grid menu's column
// picker is the only way back to them — so this checks both halves:
// what is up by default, and that a hidden one can be put back.
test("the grid menu puts back a column the log starts without", async ({ page }) => {
  const dialog = await openServerLog(page);
  const headers = dialog.locator(".rl-grid .slick-header-column");
  await expect(headers).toHaveText([
    "Time",
    "Step",
    "Level",
    "Stream",
    "Source",
    "Message",
    "Fields",
  ]);

  await dialog.locator(".slick-grid-menu-button").click();
  // The picker lists the hidden five as well, each with its box clear.
  const picker = page.locator(".slick-grid-menu .slick-column-picker-list").filter({
    hasText: "Thread",
  });
  for (const name of ["Run", "Process", "Commit", "Thread", "Target"]) {
    await expect(picker.getByLabel(name, { exact: true })).not.toBeChecked();
  }
  await expect(picker.getByLabel("Time", { exact: true })).toBeChecked();
  await picker.getByText("Thread", { exact: true }).click();
  await page.keyboard.press("Escape");

  // Thread comes back where it sits in the set, not on the end.
  await expect(headers).toHaveText([
    "Time",
    "Step",
    "Level",
    "Stream",
    "Thread",
    "Source",
    "Message",
    "Fields",
  ]);
});

// A tracing line carries the file and line that wrote it; the Source
// column shows them and, since the server knows which commit it came
// from (playwright.config.ts hands it one), links them to that line on
// GitHub. The link is the UI's to build: the store holds only the path
// rustc saw and the commit.
test("a line's source links to its file and line at the server's commit", async ({ page }) => {
  expect(GIT_HASH, "playwright.config.ts should have pinned DATALIB_GIT_HASH").toBeTruthy();
  const dialog = await openServerLog(page);
  const link = dialog.locator(`${ROWS} .slick-cell[col-id="source"] a`).first();
  await expect(link).toBeVisible();
  const shown = (await link.textContent()) ?? "";
  const m = /^(datalib\/backend\/.+\.rs):(\d+)$/.exec(shown.trim());
  expect(m, `source cell reads ${JSON.stringify(shown)}`).not.toBeNull();
  await expect(link).toHaveAttribute(
    "href",
    `https://github.com/imbue-ai/datalib/blob/${GIT_HASH}/${m![1]}#L${m![2]}`,
  );
  await expect(link).toHaveAttribute("target", "_blank");
});

// A selected line opens in full in the column after the log — the
// grid's own row-to-document pattern — and the inspector's "keep"
// narrows the log through the bus.
test("a selected line opens in full beside the log, and can narrow it", async ({ page }) => {
  const dialog = await openServerLog(page);
  const first = dialog.locator(ROWS).first();
  const msg = (await first.locator('.slick-cell[col-id="msg"]').textContent())?.trim() ?? "";
  await first.locator('.slick-cell[col-id="msg"]').click();

  const inspector = page.locator(".miller-col").filter({ has: page.locator(".ll") });
  // The card mounts in a new column after the click; on a loaded runner
  // give it what the first row got above.
  await expect(inspector).toBeVisible({ timeout: 10_000 });
  await expect(inspector.locator(".ll-msg")).toHaveText(msg);
  await expect(inspector.locator(".ll-level")).toHaveText("info");
  await expect(inspector.locator(".ll-meta")).toContainText("the server");
  // The source link, at the server's commit.
  await expect(inspector.locator(".ll-meta a.ll-link").first()).toHaveAttribute(
    "href",
    new RegExp(`^https://github.com/imbue-ai/datalib/blob/${GIT_HASH}/datalib/backend/`),
  );

  // "keep" on the thread chip narrows the log beside it.
  await inspector
    .locator(".ll-meta")
    .getByRole("button", { name: /^main$/ })
    .click();
  await expect(dialog.locator(".rl-search")).toHaveValue("min_level:info thread:main");

  // The arrow key moves the selection, and the inspector follows.
  await dialog.locator(ROWS).first().locator('.slick-cell[col-id="msg"]').click();
  await page.keyboard.press("ArrowDown");
  const second = (
    await dialog.locator(ROWS).nth(1).locator('.slick-cell[col-id="msg"]').textContent()
  )?.trim();
  await expect(inspector.locator(".ll-msg")).toHaveText(second ?? "");
});

// Grouping goes through the panel's `__fwRunLogApi.groupBy`, which
// calls the plugin's own `setDroppedGroups` — the same thing its drop
// handler calls, and how the Explore grid's spec groups too. The drag
// itself is SortableJS's native drag-and-drop, and a drag dispatched by
// hand died inside it on CI's loaded runners at more than one point
// (the header never entering the bar; the drop never ending the drag),
// through three rewrites. What is ours — the columns declared
// groupable, the placeholder, the group row's text, the toggle — is
// what this checks.
test("grouped by a column, the lines fold under group rows", async ({ page }) => {
  const dialog = await openServerLog(page);
  const bar = dialog.locator(".slick-preheader-panel .slick-dropzone");
  await expect(bar).toContainText("Drag a column here");
  await expect(dialog.locator(".slick-group-toggle-all")).toBeHidden();

  await page.evaluate(() =>
    (
      window as unknown as { __fwRunLogApi: { groupBy: (ids: string[]) => void } }
    ).__fwRunLogApi.groupBy(["level"]),
  );

  // A chip for the column takes the placeholder's place in the bar…
  await expect(bar.locator(".slick-dropped-grouping")).toContainText("Level");
  await expect(bar.locator(".slick-draggable-dropzone-placeholder")).toBeHidden();
  // …and the lines sit under group rows that say what they share and
  // how many there are.
  const group = dialog.locator(".rl-grid .slick-row.slick-group").first();
  await expect(group).toBeVisible();
  await expect(group).toHaveText(/^Level: \w+ \(\d+\)$/);
  await expect(dialog.locator(".slick-group-toggle-all")).toContainText("Expand / collapse all");
});

test("a dragged column width outlives the panel resizing", async ({ page }) => {
  const dialog = await openServerLog(page);
  const level = dialog.locator('.rl-grid .slick-header-column[col-id="level"]');
  const before = (await level.boundingBox())!.width;
  // Under the column's declared 80px, which used to be its floor. The
  // grab is on the header's own side of the handle: the half past the
  // edge sits under the next header. Retried as a whole: a drag that
  // lands while the header is still being built moves nothing.
  await expect(async () => {
    const grip = (await level.locator(".slick-resizable-handle").boundingBox())!;
    const header = (await level.boundingBox())!;
    const x = Math.min(grip.x + grip.width / 2, header.x + header.width - 2);
    const y = grip.y + grip.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x - 35, y, { steps: 5 });
    await page.mouse.up();
    expect((await level.boundingBox())!.width).toBeLessThan(before - 25);
  }, "the Level column never narrowed").toPass({ timeout: 10_000, intervals: [250, 500] });
  const dragged = (await level.boundingBox())!.width;

  // Drag the log's own column wider, the way a person does. The fit that
  // ran on every resize of the grid used to put every column back.
  const grid = dialog.locator(".rl-grid .slickgrid-container");
  const gridBefore = (await grid.boundingBox())!.width;
  const edge = (await dialog.locator(".miller-col-resize").boundingBox())!;
  const ex = edge.x + edge.width / 2;
  const ey = edge.y + edge.height / 2;
  await page.mouse.move(ex, ey);
  await page.mouse.down();
  await page.mouse.move(ex + 100, ey);
  await page.mouse.move(ex + 200, ey);
  await page.mouse.up();
  await expect
    .poll(async () => (await grid.boundingBox())!.width)
    .toBeGreaterThan(gridBefore + 100);
  expect((await level.boundingBox())!.width).toBe(dragged);
});
