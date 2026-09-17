// Streaming, watched from the two screens it is for.
//
// Two API-backed sources replay playback tapes with a delay on every
// request, so each download lasts several seconds and seals checkpoints
// on the way. What has to be true while they are still running:
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
// Both were false before the scheduler learned that an early pass of a
// middle step is a seal for the next hop (`render` ran on each `ingest`
// checkpoint, but `grid_index` heard nothing until `render` went
// terminal — after the download was over) and before the grid listened
// for the index changing at all.
//
// `qmd_index` is deliberately not in this config: its sink is an FTS
// index rewritten in place, which cannot be read mid-write, so it is a
// barrier by design and says nothing about streaming — and its model
// load is the slowest thing in the suite.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  expandGroup,
  searchAndSettle,
  settleRow,
  settleRunner,
  stampsBefore,
} from "./grid-helpers";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.DATALIB_TEST_E2E_DATALIB_STEP;
const PLAYBACK = process.env.DATALIB_TEST_E2E_PLAYBACK_DIR;

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
  await page.goto("/sources2");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
}

async function writeConfig(page: Page, text: string) {
  await openManager(page);
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
  // Remount, so the rows are painted from the config and the runner's
  // record together — see manager2-sync.spec.ts for the frame this
  // avoids.
  await openManager(page);
}

/// One reading of the Pipeline rows this spec watches: each row's
/// status word and its Activity text.
type Frame = {
  t: number;
  status: Record<string, string | null>;
  activity: Record<string, string>;
};

/// Record every distinct reading of the rows from now until the page
/// navigates. Mutation-driven rather than polled, so a frame that lasts
/// a few hundred milliseconds is still seen.
async function recordFrames(page: Page, ids: readonly string[]): Promise<void> {
  await page.evaluate((ids: string[]) => {
    const w = window as unknown as { __frames?: Frame[] };
    const frames: Frame[] = [];
    w.__frames = frames;
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
    const read = (): Frame => {
      const status: Record<string, string | null> = {};
      const activity: Record<string, string> = {};
      for (const id of ids) {
        const row = deepQuery(`.slick-row[data-key="${CSS.escape(id)}"]`);
        status[id] =
          row?.querySelector('[col-id="status"] [role="img"]')?.getAttribute("aria-label") ?? null;
        activity[id] =
          row?.querySelector('[col-id="activity"] .tg-chips')?.getAttribute("title") ?? "";
      }
      return { t: Date.now(), status, activity };
    };
    const sample = () => {
      const next = read();
      const last = frames[frames.length - 1];
      const same =
        last &&
        JSON.stringify(last.status) === JSON.stringify(next.status) &&
        JSON.stringify(last.activity) === JSON.stringify(next.activity);
      if (!same) frames.push(next);
    };
    sample();
    const observer = new MutationObserver(sample);
    for (const r of roots()) {
      observer.observe(r === document ? document.body : r, {
        subtree: true,
        childList: true,
        characterData: true,
        attributes: true,
        attributeFilter: ["aria-label", "title"],
      });
    }
  }, ids as string[]);
}

async function frames(page: Page): Promise<Frame[]> {
  return page.evaluate(() => (window as unknown as { __frames?: Frame[] }).__frames ?? []);
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
  if (!original) return;
  await writeConfig(page, original);
});

test.describe("a streaming sync, watched live", () => {
  // Two replayed downloads of a few conversations each, at 1.5 s per
  // request, plus the renders and index passes they drive.
  test.setTimeout(180_000);

  test.skip(
    !STEP_BIN || !PLAYBACK,
    "needs DATALIB_TEST_E2E_DATALIB_STEP + DATALIB_TEST_E2E_PLAYBACK_DIR from run_e2e.sh",
  );

  // Carry the `[[applets]]` stanza forward from whatever was there.
  const applets = () => {
    const at = original.indexOf("[[applets]]");
    return at === -1 ? "" : `\n${original.slice(at)}`;
  };

  // A binary path is single-quoted: a step's `command` is split
  // shell-style and the runfiles path may contain a space. The cadence
  // is short so a download of three conversations seals more than once
  // rather than only at its end.
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
at_most_every_secs = 2

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

    await recordFrames(page, STEPS);
    const was = await stampsBefore(page, STEPS);
    await page.getByRole("button", { name: "Sync everything" }).click();
    await expect(page.getByText("Queued a sync of everything.")).toBeVisible();

    // ── 2. rows arrive while their download is still running ────────
    // The instant the grid first shows a row, ask the runner what the
    // download that produced it is doing. Sampled from the test rather
    // than the page so the two readings are as close together as a
    // request allows.
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

    // ── let every row finish, then read the whole sequence back ─────
    // `settleRow` rather than `settleRows`: the recorder lives in the
    // page, and the remount `settleRunner` does would throw it away.
    for (const id of STEPS) {
      const st = await settleRow(page, id, was[id], 120_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    const seen = await frames(page);
    const describe = (f: Frame) =>
      `${new Date(f.t).toISOString().slice(11, 23)} ${JSON.stringify(f.status)} ${JSON.stringify(f.activity)}`;
    const trace = () => seen.map(describe).join("\n");
    // What the Pipeline table showed, frame by frame — in the report and
    // in bazel's test log, so a run can be read without a trace viewer.
    console.log(`[e2e] the Pipeline table, as recorded:\n${trace()}`);

    // ── 1. the Pipeline table showed the whole chain in flight ──────
    // A download counts its seals: the runner records every checkpoint
    // as a metric, and the Activity cell draws it.
    expect(
      seen.some((f) => /\bcheckpoints \d+/.test(f.activity[INGESTS[0]])),
      `${INGESTS[0]} never showed a checkpoints chip:\n${trace()}`,
    ).toBe(true);
    // Its render was Running while it was still Running.
    expect(
      seen.some((f) => f.status[RENDERS[0]] === "Running" && f.status[INGESTS[0]] === "Running"),
      `${RENDERS[0]} never ran while ${INGESTS[0]} was running:\n${trace()}`,
    ).toBe(true);
    // And the index was Running while *every* download still was — the
    // whole chain in flight from one checkpoint, not the index waiting
    // for a source to finish.
    expect(
      seen.some(
        (f) => f.status[INDEX] === "Running" && INGESTS.every((i) => f.status[i] === "Running"),
      ),
      `${INDEX} never ran while both downloads were running:\n${trace()}`,
    ).toBe(true);
    // The index's Activity says where its work came from, per producer.
    expect(
      seen.some((f) => /queued/.test(f.activity[INDEX])),
      `${INDEX} never showed its queue:\n${trace()}`,
    ).toBe(true);

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
