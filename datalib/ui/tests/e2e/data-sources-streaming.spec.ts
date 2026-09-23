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
//
// Both are states to wait for, not frames to catch: nothing upstream can
// finish while the hold is in place.
//
// `qmd_index` is deliberately not in this config: its sink is an FTS
// index rewritten in place, which cannot be read mid-write, so it is a
// barrier by design and says nothing about streaming — and its model
// load is the slowest thing in the suite.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
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
} from "./grid-helpers";

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
  const config = () => `data_root = "${dataRoot}"

[checkpoint_cadence]
at_most_every_secs = 0

[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
command = "'${STEP_BIN}'"
inputs = [${RENDERS.map((r) => `"${r}"`).join(", ")}]
${source("chatgpt-replay", "chatgpt")}${source("claude-replay", "claude")}${applets()}`;

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

    // ── let every row finish ────────────────────────────────────────
    release();
    for (const id of STEPS) {
      const st = await settleRow(page, id, was[id], 120_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }

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
  });
});
