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
// Every sync is its own request, and the server's one loop takes each on
// as it arrives, beside whatever is already running.
//
// Every test leaves no request open and the loop idle behind it: a
// request left open by one test would run against the config the next
// test writes.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import {
  expandGroup,
  groupRow,
  pickRowMenu,
  pipelineRow as row,
  rowMenuEntry,
  settleRow,
  settleRunner,
  stampOf,
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

type SyncRequest = {
  id: string;
  roots: string[];
  state: "open" | "done" | "failed" | "stopped" | "closed";
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
  // record together — see data-sources-sync.spec.ts for the frame this
  // avoids.
  await openManager(page);
}

/// The open requests, then the newest closed ones.
async function requests(request: APIRequestContext): Promise<SyncRequest[]> {
  return (await (await request.get("/api/requests")).json()) as SyncRequest[];
}

/// The newest request rooted at exactly this source's ingest step.
async function requestFor(request: APIRequestContext, s: Source): Promise<SyncRequest | undefined> {
  return (await requests(request)).find((r) => r.roots.join() === ingestOf(s));
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
/// …and on the face it shows while an open request wants it…
const stopBtn = (page: Page, rowId: string) =>
  row(page, rowId).getByRole("button", { name: /^Stop the sync/ });
/// …and on the one it shows once that request has been told to stop and
/// its steps are still winding down.
const stoppingBtn = (page: Page, rowId: string) =>
  row(page, rowId).getByRole("button", { name: /^Stopping the sync/ });

/// Start a source from its group's row, and wait for its request to have
/// been written: `click()` resolves when the event is dispatched, not
/// when the POST behind it returns.
async function start(page: Page, s: Source) {
  await syncBtn(page, `group:${s.id}`).click();
  await expect(page.getByText(/Queued a sync for/)).toBeVisible();
}

/// A row's status icon reading `word`, as a locator to wait on.
const statusFace = (page: Page, rowId: string, word: string) =>
  row(page, rowId).locator(`[col-id="status"] [role="img"][aria-label="${word}"]`);

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

/// Wait until a request has closed in one of the given states.
async function untilClosed(
  request: APIRequestContext,
  s: Source,
  states: SyncRequest["state"][],
  timeout = 45_000,
): Promise<SyncRequest> {
  let seen: SyncRequest | undefined;
  await expect
    .poll(
      async () => {
        seen = await requestFor(request, s);
        return seen?.state ?? "(no request)";
      },
      {
        timeout,
        intervals: [200],
        message: `the request for ${s.id} never closed as ${states.join("/")}`,
      },
    )
    .toMatch(new RegExp(`^(${states.join("|")})$`));
  return seen!;
}

/// What a failure about a stop would otherwise leave unsaid: the requests
/// as the API serves them, the loop's record, and the last lines this
/// spec's backend wrote — where the loop says what it sent and saw.
/// Playwright puts test stdout in the report and in bazel's test log,
/// which is the only place a CI run can be read from.
async function dumpStopEvidence(request: APIRequestContext, why: string): Promise<void> {
  try {
    const open = await requests(request);
    const dag = await (await request.get("/api/dag")).json();
    console.warn(`[e2e] ${why}: requests=${JSON.stringify(open)}`);
    console.warn(`[e2e] ${why}: dag=${JSON.stringify(dag)}`);
  } catch (e) {
    console.warn(`[e2e] ${why}: could not read the API: ${e}`);
  }
  try {
    const servers = JSON.parse(process.env.DATALIB_TEST_E2E_SERVERS ?? "[]") as {
      name: string;
      log: string;
    }[];
    const mine = servers.find((s) => s.name === "sandbox-data-sources-control");
    if (!mine) return;
    const tail = readFileSync(mine.log, "utf8").split("\n").slice(-80).join("\n");
    console.warn(`[e2e] ${why}: backend log tail:\n${tail}`);
  } catch (e) {
    console.warn(`[e2e] ${why}: could not read the backend log: ${e}`);
  }
}

/// Stop every open request and wait for the loop to go idle, so the next
/// test starts from nothing in flight.
async function drainRequests(page: Page) {
  for (const r of await requests(page.request)) {
    if (r.state === "open") {
      await page.request.post(`/api/requests/${encodeURIComponent(r.id)}/stop`);
    }
  }
  await expect
    .poll(async () => (await requests(page.request)).filter((r) => r.state === "open").length, {
      timeout: 60_000,
      intervals: [250],
      message: "a request never closed",
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
  await drainRequests(page);
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

test.describe("sources run independently", () => {
  // Three replayed downloads, held while the spec acts and finished once
  // released, plus a stop and a restart.
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
    // Saving the config rewrites the file under a live loop, which takes
    // the new config on without disturbing the first sync: its row must
    // still say Running once the table is remounted from the new config
    // — and with the tape held, it can say nothing else.
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE]);
    await untilRunning(page, ingestOf(CHATGPT), 10_000);
    const claudeWas = await stampsBefore(page, [ingestOf(CLAUDE), renderOf(CLAUDE)]);
    await start(page, CLAUDE);
    // Added to the config after this sync began, the second source still
    // runs beside the first rather than behind it (plans/supervisor.md
    // 4d), and the first is not disturbed by it.
    await untilRunning(page, ingestOf(CLAUDE), 10_000);
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
    await untilRunning(page, ingestOf(PDFS), 10_000);
    expect(await statusOf(page, ingestOf(CHATGPT))).toBe("Running");
    expect(await statusOf(page, ingestOf(CLAUDE))).toBe("Running");

    // ── every source finishes, in whatever order the loop took them ───
    release();
    for (const [id, before] of Object.entries({ ...was, ...claudeWas, ...pdfsWas })) {
      const st = await settleRow(page, id, before, 180_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    await settleRunner(page, 60_000);
    for (const s of [CHATGPT, CLAUDE, PDFS]) await untilClosed(request, s, ["done"]);
  });

  test("stopping one source mid-sync leaves the others alone, and it restarts after an edit", async ({
    page,
    request,
  }) => {
    await writeConfigAndOpen(page, [CHATGPT, CLAUDE]);
    // The requests are shared by every test on this root; only what
    // this one opens is its to count.
    const earlier = new Set((await requests(request)).map((r) => r.id));
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
    // Between the click and the step's exit the loop sends it SIGINT,
    // and the step stops at its next consistent point, commits and
    // exits — up to the 15 s grace before it is killed, and as little as
    // a fraction of a second: a held request answers the stop at once,
    // like a backoff. The row says so for as
    // long as that lasts: the button reads Stopping and takes no second
    // click. Sampled until the step has exited, and asserted only if the
    // window was wide enough to be seen at all — on a fast host it can
    // close before the first sample. The "Stopping…" banner is the
    // click handler's, painted a beat after the rows refetch that flips
    // the face, so it is only logged.
    const banner = page.getByText(/^Stopping the sync\. Steps in flight/);
    const stopping = stoppingBtn(page, `group:${CHATGPT.id}`);
    /// What the row and banner read while the wind-down was on, if a
    /// sample caught it.
    type Seen = { disabled: boolean; banner: boolean };
    let windingDown: Seen | null = null;
    let stopped: SyncRequest | undefined;
    let finished = false;
    try {
      await expect
        .poll(
          async () => {
            stopped = await requestFor(request, CHATGPT);
            const exited = (await statusOf(page, ingestOf(CHATGPT))) !== "Running";
            if (stopped?.state !== "open" && exited) return "finished";
            // Read the face without waiting for it: `isDisabled()` waits
            // for the element to exist, and the Stopping button leaves
            // the DOM the moment the step is gone — a sample that lands
            // in that gap would hang the poll until its timeout, long
            // after the step it is waiting on has exited.
            const faces = await stopping.evaluateAll((els) =>
              els.map((el) => (el as HTMLButtonElement).disabled),
            );
            if (faces.length > 0) {
              windingDown = {
                disabled: (windingDown?.disabled ?? true) && faces.every(Boolean),
                banner: (windingDown?.banner ?? false) || (await banner.isVisible()),
              };
            }
            return `winding down (${stopped?.state ?? "no request"})`;
          },
          {
            timeout: 45_000,
            intervals: [100],
            message: `the stop of ${CHATGPT.id} never finished`,
          },
        )
        .toBe("finished");
      finished = true;
    } finally {
      if (!finished) await dumpStopEvidence(request, "the stop never finished");
    }
    expect(stopped?.state, "a stop is a stop, not a failure").toBe("stopped");
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
    // …and once the step has gone, both stand down.
    await expect(banner).toBeHidden({ timeout: 10_000 });
    await expect(stopping).toHaveCount(0);
    // Its download's row says what happened: stopped — not failed,
    // nothing went wrong, and not finished, the work is not done. The
    // step answered SIGINT with a `cancelled` outcome, which the loop
    // records as `stopped`, while it goes on with the other source. A stopped download is not a fence
    // over its render (plans/supervisor.md §2.5): the held download
    // sealed nothing, so the render is up to date with what it committed
    // — or, if this root has never rendered the source, it was waiting
    // for a first seal and the stop ended that wait.
    await expect
      .poll(() => statusOf(page, ingestOf(CHATGPT)), {
        timeout: 30_000,
        intervals: [200],
        message: `${ingestOf(CHATGPT)} never settled after the stop`,
      })
      .toBe("Stopped");
    await expect
      .poll(() => statusOf(page, renderOf(CHATGPT)), { timeout: 10_000, intervals: [200] })
      .toMatch(/^(Up to date|Stopped)$/);
    // …and the group reads the same, off that child.
    await expect
      .poll(() => statusOf(page, `group:${CHATGPT.id}`), { timeout: 10_000, intervals: [200] })
      .toBe("Stopped");

    // The other source's request was never touched by the stop: it is
    // still open, and — once let go — it goes on to finish.
    const other = await requestFor(request, CLAUDE);
    expect(other?.state, `${CLAUDE.id}'s request after stopping ${CHATGPT.id}`).toBe("open");
    release();
    for (const id of [ingestOf(CLAUDE), renderOf(CLAUDE)]) {
      const st = await settleRow(page, id, was[id], 180_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    // Its rows are done; the request still owns the index pass behind them.
    await untilClosed(request, CLAUDE, ["done"], 120_000);
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
    await untilClosed(request, CHATGPT, ["done"]);
    // Two requests for this source now: the one that was stopped, and
    // the one that finished. Neither is the other's.
    const mine = (await requests(request)).filter(
      (r) => !earlier.has(r.id) && r.roots.join() === ingestOf(CHATGPT),
    );
    expect(mine.map((r) => r.state).sort()).toEqual(["done", "stopped"]);
  });
});

test.describe("steering one source among several", () => {
  test.setTimeout(300_000);

  test("a source started during another's sync runs beside it, not behind it", async ({
    page,
    request,
  }) => {
    // One loop takes on the second source while it runs the first — not
    // a second process on the same stores.
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

  test("a backlogged step can be put on ice, and taken off it", async ({ page }) => {
    // A step paused from its row's menu is not started, and what reads
    // it waits; the sync of its source runs everything else and closes.
    // Resumed, the next sync takes it on.
    await writeConfigAndOpen(page, [PDFS]);
    await pickRowMenu(page, row(page, INDEX), "Pause", statusFace(page, INDEX, "Paused"));
    expect(await statusOf(page, INDEX)).toBe("Paused");

    const was = await stampsBefore(page, [ingestOf(PDFS), renderOf(PDFS), INDEX]);
    await start(page, PDFS);
    for (const id of [ingestOf(PDFS), renderOf(PDFS)]) {
      const st = await settleRow(page, id, was[id], 120_000);
      expect(st, `${id} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
    }
    await untilClosed(page.request, PDFS, ["done"]);
    expect(await statusOf(page, INDEX)).toBe("Paused");
    expect(await stampOf(page, INDEX), "a paused step took no part").toBe(was[INDEX]);

    await (await rowMenuEntry(page, row(page, INDEX), "Resume").open()).click();
    await expect
      .poll(() => statusOf(page, INDEX), { timeout: 10_000, intervals: [200] })
      .not.toBe("Paused");
    const again = await stampsBefore(page, [INDEX]);
    await start(page, PDFS);
    const st = await settleRow(page, INDEX, again[INDEX], 120_000);
    expect(st, `${INDEX} settled as ${st}`).toMatch(/^(Succeeded|Up to date)$/);
  });
});
