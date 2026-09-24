// Streaming, watched from the two screens it is for.
//
// Two API-backed sources replay playback tapes behind a hold: while the
// hold file exists, a download that has sealed a checkpoint answers
// nothing more. Each download runs to its first seal and parks there,
// in flight with rows already published, until the test lets go. What
// has to be true while they are parked:
//
//   1. **The Pipeline table shows the whole chain in flight at once.**
//      A download's Activity cell counts its checkpoints; its render and
//      the index behind it read Running *while the download is still
//      Running* — not queued behind it.
//   2. **Rows reach the Explore grid before the download that produced
//      them finishes.** The grid was opened and searched before the sync
//      began, and is never touched again; it refetches itself when the
//      index moves.
//   3. **The Pipeline table redraws only the rows that changed.** Its
//      rows are refetched several times a second while a run goes, and
//      a row redrawn under the pointer loses the click aimed at it.
//   4. **A refresh changes only what changed.** A column the person
//      showed stays shown, and a row the index did not touch keeps its
//      element while new ones arrive.
//
// Both are states to wait for, not frames to catch: nothing upstream can
// finish while the hold is in place.
//
// `qmd_index` is deliberately not in this config: its sink is an FTS
// index rewritten in place, which cannot be read mid-write, so it is a
// barrier by design and says nothing about streaming — and its model
// load is the slowest thing in the suite.

import { test, expect, type APIRequestContext, type Locator, type Page } from "@playwright/test";
import { rmSync, writeFileSync } from "node:fs";
import {
  expandGroup,
  pipelineRow,
  searchAndSettle,
  settleRow,
  settleRunner,
  stampsBefore,
  statusOf,
  MANAGE_WITH_CONFIG,
  TABLE_ROWS,
  SEARCH_ROWS,
  type GridApi,
} from "./grid-helpers";
import { expectSanePaints, watchPaints } from "./paint-watch";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.DATALIB_TEST_E2E_DATALIB_STEP;
const PLAYBACK = process.env.DATALIB_TEST_E2E_PLAYBACK_DIR;
const HOLD = process.env.DATALIB_TEST_E2E_PLAYBACK_HOLD_SEALED;

/// Park every download of this spec's backend at its first seal.
function hold() {
  writeFileSync(HOLD!, "");
}
/// Let the parked downloads run to their end at fixture speed.
function release() {
  rmSync(HOLD!, { force: true });
}

const SOURCES = ["chatgpt-replay", "claude-replay"] as const;
const INGESTS = SOURCES.map((s) => `${s}/ingest`);
const RENDERS = SOURCES.map((s) => `${s}/render_markdown`);
const INDEX = "unified_index/grid_index";
const STEPS = [...INGESTS, ...RENDERS, INDEX];

type DagStep = { id: string; current_state: string | null };

let dataRoot = "";
async function resolveDataRoot(request: APIRequestContext): Promise<string> {
  const { path } = (await (await request.get("/api/config")).json()) as {
    path: string;
  };
  return path.slice(0, path.lastIndexOf("/"));
}

async function openManager(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
}

async function writeConfig(page: Page, text: string) {
  await openManager(page);
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
  // Remount, so the rows are painted from the config and the runner's
  // record together — see data-sources-sync.spec.ts for the frame this
  // avoids.
  await openManager(page);
}

/// One reading of the Pipeline rows this spec watches — each row's
/// status word and its Activity text — so "at once" means one reading.
async function readRows(page: Page, ids: readonly string[]) {
  const status: Record<string, string | null> = {};
  const activity: Record<string, string> = {};
  for (const id of ids) {
    status[id] = await statusOf(page, id);
    const chips = pipelineRow(page, id).locator('[col-id="activity"] .tg-chips');
    activity[id] = (await chips.count()) ? ((await chips.first().getAttribute("title")) ?? "") : "";
  }
  return { status, activity };
}

/// Mark every Pipeline row's element, let `frames` more answers for the
/// rows land, and name the rows whose element is still the one marked:
/// a row the grid redraws is a new element.
async function rowsKeptAcross(page: Page, frames: number): Promise<string[]> {
  const rows = page.locator(TABLE_ROWS);
  await rows.evaluateAll((els) => els.forEach((el) => el.setAttribute("data-probe", "")));
  for (let i = 0; i < frames; i++) {
    await page.waitForResponse((r) => r.url().includes("/api/manage/rows"), { timeout: 30_000 });
  }
  // The last answer is committed after it arrives, and painted after that.
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => r(null))));
  return rows.evaluateAll((els) =>
    els.filter((el) => el.hasAttribute("data-probe")).map((el) => el.dataset.key ?? ""),
  );
}
/// The Explore grid's hidden columns.
const hiddenColumns = (grid: Page) =>
  grid.evaluate(() => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.hiddenColumns());

/// How many lines a log panel says it holds.
const logLineCount = (log: Locator) =>
  log
    .locator(".rl-count")
    .evaluate((el) => Number(/(\d+) line/.exec(el.textContent ?? "")?.[1] ?? NaN));

/// What the runner says each step is doing right now.
async function currentStates(page: Page): Promise<Record<string, string | null>> {
  const dag = (await (await page.request.get("/api/dag")).json()) as { steps?: DagStep[] };
  return Object.fromEntries((dag.steps ?? []).map((s) => [s.id, s.current_state]));
}

/// The Explore grid's row count, off its own status line.
async function gridRowCount(grid: Page): Promise<number> {
  const text = await grid.locator(".status").first().innerText();
  const m = /^(\d+) rows/.exec(text.trim());
  return m ? Number(m[1]) : 0;
}

let original = "";

test.beforeEach(async ({ page, request }) => {
  dataRoot = await resolveDataRoot(request);
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  release();
  if (!original) return;
  await writeConfig(page, original);
});

test.describe("a streaming sync, watched live", () => {
  // Two replayed downloads of a few conversations each, plus the renders
  // and index passes they drive.
  test.setTimeout(180_000);

  test.skip(
    !STEP_BIN || !PLAYBACK || !HOLD,
    "needs DATALIB_TEST_E2E_DATALIB_STEP + DATALIB_TEST_E2E_PLAYBACK_DIR + DATALIB_TEST_E2E_PLAYBACK_HOLD_SEALED from run_e2e.sh",
  );

  // Carry the `[[applets]]` stanza forward from whatever was there.
  const applets = () => {
    const at = original.indexOf("[[applets]]");
    return at === -1 ? "" : `\n${original.slice(at)}`;
  };

  // A binary path is single-quoted: a step's `command` is split
  // shell-style and the runfiles path may contain a space. A cadence of
  // zero seals after every conversation, so each download's first seal
  // comes after its first conversation however fast the tape plays.
  const source = (id: string, type: string) => `
[[groups]]
id = "${id}"
type = "${type}"
name = "${type} (replayed)"

[[steps]]
group = "${id}"
function = "ingest"
command = "'${STEP_BIN}'"
[steps.params.api]

[[steps]]
group = "${id}"
function = "render_markdown"
command = "'${STEP_BIN}'"
inputs = ["${id}/ingest"]
`;
  const config = (
    sources: readonly (readonly [string, string])[] = [
      [SOURCES[0], "chatgpt"],
      [SOURCES[1], "claude"],
    ],
  ) => `data_root = "${dataRoot}"

[checkpoint_cadence]
at_most_every_secs = 0

[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
command = "'${STEP_BIN}'"
inputs = [${sources.map(([id]) => `"${id}/render_markdown"`).join(", ")}]
${sources.map(([id, type]) => source(id, type)).join("")}${applets()}`;

  test("every stage is in flight at once, and rows reach the grid mid-download", async ({
    page,
    context,
  }) => {
    await writeConfig(page, config());
    for (const id of [...SOURCES, "unified_index"]) await expandGroup(page, id);

    // The Explore grid, in a second tab, filtered to the first source's
    // rows before it has any. Not touched again after this: the point
    // is that it updates itself.
    const grid = await context.newPage();
    await grid.goto("/");
    await searchAndSettle(grid, `source_id:${SOURCES[0]}`);
    await expect(grid.getByText("no matches.")).toBeVisible();

    const was = await stampsBefore(page, STEPS);
    const pipelinePaints = await watchPaints(page.locator(".tg-grid").first());
    hold();
    await page.getByRole("button", { name: "Sync everything" }).click();
    await expect(page.getByText("Queued a sync of everything.")).toBeVisible();

    // ── 2. rows arrive while their download is still running ────────
    await expect
      .poll(() => gridRowCount(grid), {
        timeout: 60_000,
        intervals: [100],
        message: "no row ever reached the grid",
      })
      .toBeGreaterThan(0);
    const firstRows = await gridRowCount(grid);
    const when = await currentStates(page);
    console.log(
      `[e2e] ${new Date().toISOString().slice(11, 23)} the grid shows ${firstRows} row(s); ` +
        `runner: ${JSON.stringify(when)}`,
    );
    expect(
      when[INGESTS[0]],
      `the grid showed ${firstRows} row(s) from ${SOURCES[0]} only once its download was over ` +
        `(runner: ${JSON.stringify(when)})`,
    ).toBe("running");

    // ── 4. the person's columns outlive a refresh ───────────────────
    // Every row is from one account, so the grid hides Account on its
    // own. Show it the way the column picker does, and mark the rows'
    // elements to see which ones the refreshes below redraw.
    expect(await hiddenColumns(grid)).toContain("account");
    await grid.evaluate(() =>
      (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.showColumns(["account"]),
    );
    expect(await hiddenColumns(grid)).not.toContain("account");
    const explorePaints = await watchPaints(grid.locator(".grid-box"));
    await grid
      .locator(SEARCH_ROWS)
      .evaluateAll((els) => els.forEach((el) => el.setAttribute("data-probe", "")));

    // ── 1. the Pipeline table shows the whole chain in flight ───────
    // Every row Running in one reading: both downloads, the render
    // behind each, and the index behind both. A download counts its
    // seals (the runner records every checkpoint as a metric, and the
    // Activity cell draws it); the index says where its work came from.
    let last = await readRows(page, STEPS);
    const inFlight = () =>
      STEPS.every((id) => last.status[id] === "Running") &&
      /\bcheckpoints \d+/.test(last.activity[INGESTS[0]]) &&
      /queued/.test(last.activity[INDEX]);
    await expect
      .poll(
        async () => {
          last = await readRows(page, STEPS);
          return inFlight();
        },
        {
          timeout: 60_000,
          intervals: [250],
          message: "the whole chain was never in flight at once",
        },
      )
      .toBe(true)
      .catch((e: Error) => {
        throw new Error(`${e.message}\nlast reading: ${JSON.stringify(last, null, 2)}`);
      });

    // ── 3. a refetch redraws only what changed ──────────────────────
    const kept = await rowsKeptAcross(page, 3);
    console.log(`[e2e] rows kept across three refetches: ${JSON.stringify(kept)}`);
    expect(kept, "every Pipeline row was redrawn by a refetch").toContain(`group:${SOURCES[0]}`);

    // ── let every row finish ────────────────────────────────────────
    release();
    for (const id of STEPS) {
      const st = await settleRow(page, id, was[id], 120_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    // Before `settleRunner`, which reloads the page.
    expectSanePaints(await pipelinePaints(), "the Pipeline table, through a streaming sync");

    // ── and the grid caught up with every pass, not just the first ──
    // Every conversation the tapes hold, once the run is over: the same
    // count the applet gives a fresh request.
    await settleRunner(page, 120_000);
    const search = (await (
      await page.request.get(
        `/applet/unified_index/search?q=${encodeURIComponent(`source_id:${SOURCES[0]}`)}&limit=1000`,
      )
    ).json()) as { rows: unknown[] };
    expect(search.rows.length, "the tapes hold more than one conversation").toBeGreaterThan(
      firstRows,
    );
    await expect
      .poll(() => gridRowCount(grid), {
        timeout: 15_000,
        message: "the grid stopped following the index after its first refresh",
      })
      .toBe(search.rows.length);
    expect(
      await hiddenColumns(grid),
      "a refresh of the same query hid a column the person showed",
    ).not.toContain("account");
    const keptSearch = await grid
      .locator(`${SEARCH_ROWS}[data-probe]`)
      .evaluateAll((els) => els.map((el) => el.getAttribute("data-row")));
    console.log(`[e2e] search rows kept across the refreshes: ${JSON.stringify(keptSearch)}`);
    expect(keptSearch.length, "a refresh of the same query redrew every row").toBeGreaterThan(0);
    expectSanePaints(await explorePaints(), "the Explore grid, through a streaming sync");
  });

  // The panel follows its tail as a running step writes. A person
  // reading a line has a menu open on it, and the lines arriving below
  // must neither close that menu nor redraw the line under it.
  test("a running step's log holds still while a menu is open on it", async ({ page }) => {
    // A source of its own: one the test above already fetched is up to
    // date, and never runs.
    const id = "chatgpt-log";
    const ingest = `${id}/ingest`;
    const steps = [ingest, `${id}/render_markdown`, INDEX];
    await writeConfig(page, config([[id, "chatgpt"]]));
    for (const group of [id, "unified_index"]) await expandGroup(page, group);
    const was = await stampsBefore(page, steps);
    hold();
    await page.getByRole("button", { name: "Sync everything" }).click();
    await expect
      .poll(() => statusOf(page, ingest), { timeout: 60_000, intervals: [250] })
      .toBe("Running");

    await pipelineRow(page, ingest).locator('[col-id="status"] .tg-status').dblclick();
    const log = page.locator(".miller-col").filter({ has: page.locator(".rl-panel") });
    const lines = log.locator(".rl-grid .slick-row:not(.slick-group)");
    await expect(lines.first()).toBeVisible({ timeout: 10_000 });
    const before = await logLineCount(log);

    const menu = page.locator(".slick-context-menu");
    await expect(async () => {
      await lines.first().locator(".slick-cell.l0").click({ button: "right", timeout: 1_000 });
      await expect(menu).toBeVisible({ timeout: 1_000 });
    }).toPass();
    const paints = await watchPaints(log.locator(".rl-grid"));

    // The download runs to its end, writing lines the whole way, while
    // the menu stays open on the first one.
    release();
    await settleRow(page, ingest, was[ingest], 120_000);
    await expect(menu, "a line arriving closed the menu").toBeVisible();
    expectSanePaints(await paints(), "the log of a running step, under an open menu");

    // Closed, the menu lets the lines it held back in.
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect
      .poll(() => logLineCount(log), {
        timeout: 30_000,
        message: "the lines held back while the menu was open never arrived",
      })
      .toBeGreaterThan(before);
    for (const step of steps) await settleRow(page, step, was[step], 120_000);
    await settleRunner(page, 120_000);
  });
});
