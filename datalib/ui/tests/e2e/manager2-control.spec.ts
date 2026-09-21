// Running sources independently from the Pipeline table: start one,
// start another while the first is still going, stop one of them, edit
// it, start it again — the way a person actually sets a data root up,
// one source at a time, with the earlier ones still syncing.
//
// The sources replay playback tapes behind a hold: while the hold file
// exists no replayed request is answered, so a started download stays
// in flight for exactly as long as the test needs to act on it, and
// finishes at once when the test lets go. No window to miss on a slow
// runner, and no sleeping on a fast one.
//
// Two groups of tests. The first is the workflow as the tree supports it
// today — every sync is its own job and the worker runs them one at a
// time, so a source started during another's sync waits its turn — and
// has to stay green. The second is what the workflow needs and does not
// have yet, marked `test.fail()` or `test.fixme()`: an expected failure
// passes today, and the day the feature lands Playwright fails it as
// "expected to fail, but passed", which is the reminder to turn it into
// a plain test.
//
// Every test leaves the queue empty and the root unlocked behind it: a
// job left pending by one test would run against the config the next
// test writes.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import {
  expandGroup,
  groupRow,
  pipelineRow as row,
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
const PDF_DIR = process.env.DATALIB_TEST_E2E_PDF_FIXTURE_DIR;
const HOLD = process.env.DATALIB_TEST_E2E_PLAYBACK_HOLD;

/// Park every replayed request of this spec's backend until `release`.
function hold() {
  writeFileSync(HOLD!, "");
}
/// Let the held downloads run to their end at fixture speed.
function release() {
  rmSync(HOLD!, { force: true });
}

/// The sources this file starts and stops. Each replays one tape, and
/// the two tapes are the two API-backed providers the harness
/// synthesizes; `pdfs` reads a local corpus and finishes at once.
type Source = { id: string; type: string; params: string };
const CHATGPT: Source = { id: "chatgpt-replay", type: "chatgpt", params: "[steps.params.api]" };
const CLAUDE: Source = { id: "claude-replay", type: "claude", params: "[steps.params.api]" };
const PDFS: Source = {
  id: "pdfs",
  type: "pdf",
  params: `[steps.params.fswalk]\npath = "${PDF_DIR}"`,
};
const INDEX = "unified_index/grid_index";
const ingestOf = (s: Source) => `${s.id}/ingest`;
const renderOf = (s: Source) => `${s.id}/render_markdown`;

type SyncJob = {
  id: string;
  source_ids: string | null;
  state: "pending" | "running" | "done" | "failed" | "canceled";
  /// The server's word on whether the job still holds the runner: a
  /// cancel flips `state` at once, and this only once the worker has
  /// stamped the job finished.
  active: boolean;
};
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
  // record together — see manager2-sync.spec.ts for the frame this
  // avoids.
  await openManager(page);
}

/// Every job the queue holds, newest first.
async function jobs(request: APIRequestContext): Promise<SyncJob[]> {
  return (await (await request.get("/api/sync/jobs/all")).json()) as SyncJob[];
}

/// The newest job whose seeds are exactly this source's ingest step.
async function jobFor(request: APIRequestContext, s: Source): Promise<SyncJob | undefined> {
  return (await jobs(request)).find((j) => j.source_ids === ingestOf(s));
}

/// What the runner says each step is doing right now.
async function currentStates(request: APIRequestContext): Promise<Record<string, string | null>> {
  const dag = (await (await request.get("/api/dag")).json()) as { steps?: DagStep[] };
  return Object.fromEntries((dag.steps ?? []).map((s) => [s.id, s.current_state]));
}

/// The one button a row keeps, on the face it shows while nothing has
/// the row claimed…
const syncBtn = (page: Page, rowId: string) =>
  row(page, rowId).getByRole("button", { name: "Sync now" });
/// …and on the face it shows while a job does…
const stopBtn = (page: Page, rowId: string) =>
  row(page, rowId).getByRole("button", { name: /^Stop the sync/ });
/// …and on the one it shows once that job has been told to stop and is
/// still winding down.
const stoppingBtn = (page: Page, rowId: string) =>
  row(page, rowId).getByRole("button", { name: /^Stopping the sync/ });

/// Start a source from its group's row, and wait for the queue to have
/// taken it: `click()` resolves when the event is dispatched, not when
/// the enqueue behind it returns.
async function start(page: Page, s: Source) {
  await syncBtn(page, `group:${s.id}`).click();
  await expect(page.getByText(/Queued a sync for/)).toBeVisible();
}

/// Wait until the runner has this source's download in flight, as the
/// row paints it.
async function untilRunning(page: Page, id: string, timeout = 45_000) {
  await expect
    .poll(() => statusOf(page, id), {
      timeout,
      intervals: [200],
      message: `${id} never reached Running`,
    })
    .toBe("Running");
}

/// Wait until a job has *finished* in one of the given states — the
/// worker has stamped it, so nothing is still running on its behalf.
/// A cancel flips the state the moment it is asked for; `active` is
/// what says the runner has actually gone.
async function untilJobFinished(
  request: APIRequestContext,
  s: Source,
  states: SyncJob["state"][],
  timeout = 45_000,
): Promise<SyncJob> {
  let seen: SyncJob | undefined;
  await expect
    .poll(
      async () => {
        seen = await jobFor(request, s);
        return seen ? `${seen.state}${seen.active ? " (still active)" : ""}` : "(no job)";
      },
      {
        timeout,
        intervals: [200],
        message: `the job for ${s.id} never finished as ${states.join("/")}`,
      },
    )
    .toMatch(new RegExp(`^(${states.join("|")})$`));
  return seen!;
}

/// What a failure about a stop would otherwise leave unsaid: the queue
/// as the API serves it, the runner's record, and the last lines this
/// spec's backend wrote — where the worker says what it sent and saw.
/// Playwright puts test stdout in the report and in bazel's test log,
/// which is the only place a CI run can be read from.
async function dumpStopEvidence(request: APIRequestContext, why: string): Promise<void> {
  try {
    const queue = await jobs(request);
    const dag = await (await request.get("/api/dag")).json();
    console.warn(`[e2e] ${why}: jobs=${JSON.stringify(queue)}`);
    console.warn(`[e2e] ${why}: dag=${JSON.stringify(dag)}`);
  } catch (e) {
    console.warn(`[e2e] ${why}: could not read the API: ${e}`);
  }
  try {
    const servers = JSON.parse(process.env.DATALIB_TEST_E2E_SERVERS ?? "[]") as {
      name: string;
      log: string;
    }[];
    const mine = servers.find((s) => s.name === "sandbox-manager2-control");
    if (!mine) return;
    const tail = readFileSync(mine.log, "utf8").split("\n").slice(-80).join("\n");
    console.warn(`[e2e] ${why}: backend log tail:\n${tail}`);
  } catch (e) {
    console.warn(`[e2e] ${why}: could not read the backend log: ${e}`);
  }
}

/// Empty the queue and wait for the runner to let go of the root, so
/// the next test starts from nothing in flight.
async function drainQueue(page: Page) {
  for (const j of await jobs(page.request)) {
    if (j.active) {
      await page.request.post(`/api/sync/jobs/${encodeURIComponent(j.id)}/cancel`);
    }
  }
  await expect
    .poll(async () => (await jobs(page.request)).filter((j) => j.active).length, {
      timeout: 60_000,
      intervals: [250],
      message: "the queue never emptied",
    })
    .toBe(0);
  await settleRunner(page, 60_000);
}

let original = "";

test.beforeEach(async ({ page, request }) => {
  hold();
  dataRoot = await resolveDataRoot(request);
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  release();
  await drainQueue(page);
  if (original) await writeConfig(page, original);
});

test.skip(
  !STEP_BIN || !PLAYBACK || !PDF_DIR || !HOLD,
  "needs DATALIB_TEST_E2E_DATALIB_STEP + DATALIB_TEST_E2E_PLAYBACK_DIR + DATALIB_TEST_E2E_PDF_FIXTURE_DIR + DATALIB_TEST_E2E_PLAYBACK_HOLD from run_e2e.sh",
);

// Carry the `[[applets]]` stanza forward from whatever was there.
const applets = () => {
  const at = original.indexOf("[[applets]]");
  return at === -1 ? "" : `\n${original.slice(at)}`;
};

// A binary path is single-quoted: a step's `command` is split
// shell-style and the runfiles path may contain a space. The cadence is
// short so a download seals more than once rather than only at its end.
const stanza = (s: Source, name = `${s.type} (replayed)`) => `
[[groups]]
id = "${s.id}"
type = "${s.type}"
name = "${name}"

[[steps]]
group = "${s.id}"
function = "ingest"
command = "'${STEP_BIN}'"
${s.params}

[[steps]]
group = "${s.id}"
function = "render_markdown"
command = "'${STEP_BIN}'"
inputs = ["${ingestOf(s)}"]
`;
const config = (sources: Source[], names: Record<string, string> = {}) => `data_root = "${dataRoot}"

[checkpoint_cadence]
at_most_every_secs = 2

[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
command = "'${STEP_BIN}'"
inputs = [${sources.map((s) => `"${renderOf(s)}"`).join(", ")}]
${sources.map((s) => stanza(s, names[s.id])).join("")}${applets()}`;

async function writeConfigAndOpen(page: Page, sources: Source[], names?: Record<string, string>) {
  await writeConfig(page, config(sources, names));
  for (const s of sources) await expandGroup(page, s.id);
  await expandGroup(page, "unified_index");
}

test.describe("sources run independently, one job at a time", () => {
  // Three replayed downloads, held while the spec acts and run one after
  // another by the worker once released, plus a stop and a restart.
  test.setTimeout(300_000);

  test("sources are added and started one at a time, each while the earlier ones still run", async ({
    page,
    request,
  }) => {
    // ── 1. start the first source ─────────────────────────────────────
    await writeConfigAndOpen(page, [CHATGPT]);
    const was = await stampsBefore(page, [ingestOf(CHATGPT), renderOf(CHATGPT), INDEX]);
    await start(page, CHATGPT);
    await untilRunning(page, ingestOf(CHATGPT));
    // While it runs, its row offers to stop it — and nothing else does.
    await expect(stopBtn(page, `group:${CHATGPT.id}`)).toBeVisible();

    // ── 2. add a second source while the first is still going ─────────
    // Saving the config rewrites the file under a live runner. The
    // runner read it at start and must not notice; the row must still
    // say Running once the table is remounted from the new config — and
    // with the tape held, it can say nothing else.
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE]);
    await untilRunning(page, ingestOf(CHATGPT), 10_000);
    const claudeWas = await stampsBefore(page, [ingestOf(CLAUDE), renderOf(CLAUDE)]);
    await start(page, CLAUDE);
    // The second source is taken on at once — queued behind the first
    // today, running beside it once the runner can (the test.fail below
    // says which) — and the first is not disturbed by it.
    await expect
      .poll(() => statusOf(page, ingestOf(CLAUDE)), { timeout: 5_000 })
      .toMatch(/^(Queued|Running)$/);
    expect(await statusOf(page, ingestOf(CHATGPT))).toBe("Running");
    await expect(stopBtn(page, `group:${CLAUDE.id}`)).toBeVisible();
    console.log(
      `[e2e] with ${CHATGPT.id} running, ${CLAUDE.id} reads ${await statusOf(page, ingestOf(CLAUDE))}; ` +
        `runner: ${JSON.stringify(await currentStates(request))}`,
    );

    // ── 3. a third, while the other two are both in flight ────────────
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE, PDFS]);
    await untilRunning(page, ingestOf(CHATGPT), 10_000);
    const pdfsWas = await stampsBefore(page, [ingestOf(PDFS), renderOf(PDFS)]);
    await start(page, PDFS);
    await expect
      .poll(() => statusOf(page, ingestOf(PDFS)), { timeout: 5_000 })
      .toMatch(/^(Queued|Running)$/);
    expect(await statusOf(page, ingestOf(CHATGPT))).toBe("Running");
    expect(await statusOf(page, ingestOf(CLAUDE))).toMatch(/^(Queued|Running)$/);

    // ── every source finishes, in whatever order the worker took them ─
    release();
    for (const [id, before] of Object.entries({ ...was, ...claudeWas, ...pdfsWas })) {
      const st = await settleRow(page, id, before, 180_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    await settleRunner(page, 60_000);
    const finished = await jobs(request);
    for (const s of [CHATGPT, CLAUDE, PDFS]) {
      expect(finished.find((j) => j.source_ids === ingestOf(s))?.state, `${s.id}'s job`).toBe(
        "done",
      );
    }
  });

  test("stopping one source mid-sync leaves the others alone, and it restarts after an edit", async ({
    page,
    request,
  }) => {
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE]);
    // The queue is shared by every test on this root; only what this
    // one enqueues is its to count.
    const earlier = new Set((await jobs(request)).map((j) => j.id));
    const was = await stampsBefore(page, [
      ingestOf(CHATGPT),
      renderOf(CHATGPT),
      ingestOf(CLAUDE),
      renderOf(CLAUDE),
      INDEX,
    ]);
    await start(page, CHATGPT);
    await untilRunning(page, ingestOf(CHATGPT));
    await start(page, CLAUDE);
    await expect
      .poll(() => statusOf(page, ingestOf(CLAUDE)), { timeout: 5_000 })
      .toMatch(/^(Queued|Running)$/);

    // ── 4. stop the first, from its own row ───────────────────────────
    await stopBtn(page, `group:${CHATGPT.id}`).click();
    // Between the click and the runner's exit the worker sends SIGTERM,
    // the runner forwards SIGINT, and the step stops at its next
    // consistent point, commits and exits — up to the worker's 15 s
    // grace, and as little as a fraction of a second: a held request
    // answers the stop at once, like a backoff. The row says so for as
    // long as that lasts: the button reads Stopping and takes no second
    // click. Sampled until the job is stamped, and asserted only if the
    // window was wide enough to be seen at all — on a fast host it can
    // close before the first sample. The "Stopping…" banner is the
    // click handler's, painted a beat after the rows refetch that flips
    // the face, so it is only logged.
    const banner = page.getByText(`Stopping the sync of ${ingestOf(CHATGPT)}`);
    const stopping = stoppingBtn(page, `group:${CHATGPT.id}`);
    /// What the row and banner read while the wind-down was on, if a
    /// sample caught it.
    type Seen = { disabled: boolean; banner: boolean };
    let windingDown: Seen | null = null;
    let stopped: SyncJob | undefined;
    let finished = false;
    try {
      await expect
        .poll(
          async () => {
            stopped = await jobFor(request, CHATGPT);
            if (stopped && !stopped.active) return "finished";
            // Read the face without waiting for it: `isDisabled()` waits
            // for the element to exist, and the Stopping button leaves
            // the DOM the moment the runner is gone — a sample that lands
            // in that gap would hang the poll until its timeout, long
            // after the job it is waiting on has finished.
            const faces = await stopping.evaluateAll((els) =>
              els.map((el) => (el as HTMLButtonElement).disabled),
            );
            if (faces.length > 0) {
              windingDown = {
                disabled: (windingDown?.disabled ?? true) && faces.every(Boolean),
                banner: (windingDown?.banner ?? false) || (await banner.isVisible()),
              };
            }
            return `winding down (${stopped?.state ?? "no job"})`;
          },
          {
            timeout: 45_000,
            intervals: [100],
            message: `the job for ${CHATGPT.id} never finished`,
          },
        )
        .toBe("finished");
      finished = true;
    } finally {
      if (!finished) await dumpStopEvidence(request, "the stop never finished");
    }
    expect(stopped?.state, "a stop is a cancel, not a failure").toBe("canceled");
    // Read through a closure: TypeScript narrows the `let` to `null`
    // here, not seeing the assignment inside the poll, and an assigned
    // local would inherit that narrowing.
    const seen = ((): Seen | null => windingDown)();
    console.log(
      `[e2e] the stop of ${CHATGPT.id} ` +
        (seen
          ? `showed its wind-down (banner ${seen.banner ? "seen" : "not seen"})`
          : "was over before a sample saw it"),
    );
    if (seen)
      expect(seen.disabled, "while winding down, Stopping takes no second click").toBe(true);
    // …and once the runner has gone, both stand down.
    await expect(banner).toBeHidden({ timeout: 10_000 });
    await expect(stopping).toHaveCount(0);
    // Its download's row says what happened: stopped — not failed,
    // nothing went wrong, and not finished, the work is not done. The
    // step answered SIGINT with a `cancelled` outcome, which the
    // scheduler records as `stopped`; its render step, never reached,
    // is blocked on it.
    await expect
      .poll(() => statusOf(page, ingestOf(CHATGPT)), {
        timeout: 30_000,
        intervals: [200],
        message: `${ingestOf(CHATGPT)} never settled after the stop`,
      })
      .toBe("Stopped");
    await expect
      .poll(() => statusOf(page, renderOf(CHATGPT)), { timeout: 10_000, intervals: [200] })
      .toBe("Blocked");
    // …and the group reads the same, off that child.
    await expect
      .poll(() => statusOf(page, `group:${CHATGPT.id}`), { timeout: 10_000, intervals: [200] })
      .toBe("Stopped");

    // The other source's job was never touched by the stop: it is still
    // in the queue, and — once let go — it goes on to finish.
    const other = await jobFor(request, CLAUDE);
    expect(other?.state, `${CLAUDE.id}'s job after stopping ${CHATGPT.id}`).toMatch(
      /^(pending|running)$/,
    );
    release();
    for (const id of [ingestOf(CLAUDE), renderOf(CLAUDE)]) {
      const st = await settleRow(page, id, was[id], 180_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    // Its rows are done; the job still owns the index pass behind them.
    await untilJobFinished(request, CLAUDE, ["done"], 120_000);
    await settleRunner(page, 60_000);

    // ── edit the stopped source, then start it again ──────────────────
    // The edit is what a person would do here — narrow the source —
    // and for a replayed tape the only visible knob is its name. What
    // is asserted is that a stopped source is startable again and
    // finishes, picking up from the checkpoint the stop left.
    const renamed = "chatgpt (narrowed)";
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE], { [CHATGPT.id]: renamed });
    await expect(groupRow(page, CHATGPT.id)).toContainText(renamed);
    const again = await stampsBefore(page, [ingestOf(CHATGPT), renderOf(CHATGPT), INDEX]);
    await start(page, CHATGPT);
    // Released, the restart is over in well under a second — too quick
    // for a poll to see it Running. The stamps moving is the proof it ran.
    for (const id of [ingestOf(CHATGPT), renderOf(CHATGPT), INDEX]) {
      const st = await settleRow(page, id, again[id], 180_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    await settleRunner(page, 60_000);
    // Two jobs for this source now: the one that was stopped, and the
    // one that finished. Neither is the other's.
    const mine = (await jobs(request)).filter(
      (j) => !earlier.has(j.id) && j.source_ids === ingestOf(CHATGPT),
    );
    expect(mine.map((j) => j.state).sort()).toEqual(["canceled", "done"]);
  });
});

test.describe("what independent control still needs", () => {
  test.setTimeout(300_000);

  test("a source started during another's sync runs beside it, not behind it", async ({
    page,
    request,
  }) => {
    // Every sync is its own `datalib-dag`, and the runner holds an
    // exclusive lock on the root, so the worker runs jobs one at a time:
    // the second source waits until the first is over. Running beside it
    // means one runner taking on new seeds while it runs — not a second
    // process on the same stores.
    test.fail();
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE]);
    await start(page, CHATGPT);
    await untilRunning(page, ingestOf(CHATGPT));
    await start(page, CLAUDE);
    // The first download is held, so there is no window to miss: the
    // second runs beside it or not at all.
    await untilRunning(page, ingestOf(CLAUDE), 3_000);
    const when = await currentStates(request);
    expect(when[ingestOf(CHATGPT)], `runner: ${JSON.stringify(when)}`).toBe("running");
    expect(when[ingestOf(CLAUDE)], `runner: ${JSON.stringify(when)}`).toBe("running");
  });

  test.fixme("a backlogged step can be put on ice, and taken off it", async ({ page }) => {
    // There is no way to say "keep this step, but don't run it" in the
    // config: a step is either in the pipeline or deleted from it, and
    // deleting the index step drops its rows from the grid. The shape
    // this wants is a flag on the step — `paused = true`, say — that the
    // loader keeps in the graph, the scheduler skips (its dependents
    // wait, not fail), the Status column paints as "Paused", and the
    // row's menu toggles.
    await writeConfigAndOpen(page, [CHATGPT]);
    await expect(row(page, INDEX)).toBeVisible();
  });
});
