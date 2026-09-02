// Driving a real sync from the Pipeline grid, and watching the whole
// sequence it produces.
//
// The unit suite (src/config/pipelineStatus.test.ts) replays synthetic
// frames through the same state machine, which pins what each frame
// *means*. It cannot tell you the frames a real backend emits, or in
// what order. That is this file's job: one real `datalib-dag` run,
// driven from the button a person presses, sampled the way the grid
// itself sees it.
//
// The pipeline is real and entirely local: no network, no credentials.
//
//   pdfs/raw -> pdfs/rendered_md    the `pdf` provider, over the
//                                   checked-in TNG corpus
//   docs/raw                        the `fsindex` scanner, over the
//                                   tree materialize_tng_root.sh drops
//                                   into every fixture root
//
// `pdf` is the local-only provider that has *both* halves, which is why
// it carries this spec: a `raw -> rendered_md` edge is what makes
// "everything downstream is queued too" a real assertion about the DAG
// rather than a contrived one. `fsindex` (download-only) is the
// unrelated second source — the one whose history must not move.
//
// Both are read-only against what they scan: `pdf` keys documents on
// content hash and writes only into its raw store, and `fsindex`'s
// breadcrumb `stamp` defaults off. The binary and the corpus arrive as
// FW_E2E_DATALIB_STEP / FW_E2E_PDF_FIXTURE_DIR, because the fixture
// root is a temp dir with nothing on PATH.
//
// Two properties are worth a real backend, and both were real bugs:
//
//   * **A sync of one source must not touch another's history.** Every
//     run walks the whole graph to publish output versions, and it used
//     to write `not_selected` into the steps it walked past — so a
//     source that succeeded yesterday came back as "not selected",
//     stamped with the time of a run that never touched it.
//   * **The row must show the sync happening.** "Running" only reached
//     `dag_state.json` when a step *finished*, so pressing Sync looked
//     like nothing had happened until it was over.
//
// The config is shared by every spec in the run (workers: 1), so it is
// restored in afterEach — including on failure.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.FW_E2E_DATALIB_STEP;
const PDF_DIR = process.env.FW_E2E_PDF_FIXTURE_DIR;

/// This spec's own data root, asked of the backend rather than read
/// from the environment.
///
/// It rewrites `config.toml`, so it runs against a data root nobody
/// else touches (`tests/e2e/config-mutating.ts`; the project's
/// `baseURL` is what points it there). The config it writes names
/// `data_root` absolutely, so it has to name *that* root — pointing at
/// the shared fixture root instead would have every sync here writing
/// into the tree twenty other specs are reading.
///
/// `GET /api/config` reports the absolute path of the config file the
/// backend is serving, so its directory is the answer, and it comes
/// from the same server the page is talking to by construction.
let dataRoot = "";
async function resolveDataRoot(request: APIRequestContext): Promise<string> {
  const { path } = (await (await request.get("/api/config")).json()) as {
    path: string;
  };
  return path.slice(0, path.lastIndexOf("/"));
}

const row = (page: Page, id: string) => page.locator(`.ag-row[row-id="${id}"]`);
const statusIcon = (page: Page, id: string) =>
  row(page, id).locator('[col-id="status"] [role="img"]');
const syncBtn = (page: Page, id: string) =>
  row(page, id).getByRole("button", { name: "Sync now" });

/// The Status column is icons, so a row's state is its accessible name.
/// Returns null while the cell is mid-repaint or the row is offscreen.
async function statusOf(page: Page, id: string): Promise<string | null> {
  const el = statusIcon(page, id);
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("aria-label");
}

/// Record every status a row *passes through*, from now until the page
/// navigates.
///
/// Sampling on a timer — which is what this replaces, a `for(;;)` loop
/// with a `waitForTimeout(150)` in it — can only see the states that
/// happen to be on screen when it looks. A step that runs for less than
/// one sampling interval is invisible, which is why the assertion below
/// had to treat "Running" as optional: the test could not tell a status
/// that never appeared from one it blinked past. Observing mutations
/// instead makes the sequence *complete*, so "Running" can be required.
///
/// Watches `aria-label` because that is where the state lives (the
/// column paints icons), and `childList` because AG Grid rebuilds a
/// cell's DOM on `refreshCells` rather than mutating it in place.
async function recordStatuses(page: Page, ids: string[]) {
  await page.evaluate((ids: string[]) => {
    const w = window as unknown as { __statusLog?: Record<string, string[]> };
    const log: Record<string, string[]> = {};
    w.__statusLog = log;
    const sample = () => {
      for (const id of ids) {
        const el = document.querySelector(
          `.ag-row[row-id="${CSS.escape(id)}"] [col-id="status"] [role="img"]`,
        );
        const s = el?.getAttribute("aria-label");
        const seen = (log[id] ??= []);
        if (s && seen[seen.length - 1] !== s) seen.push(s);
      }
    };
    sample();
    new MutationObserver(sample).observe(document.body, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["aria-label"],
    });
  }, ids);
}

/// What `recordStatuses` has seen for one row, oldest first.
async function statusLog(page: Page, id: string): Promise<string[]> {
  return page.evaluate(
    (id: string) =>
      (window as unknown as { __statusLog?: Record<string, string[]> }).__statusLog?.[id] ?? [],
    id,
  );
}

/// "Last synced" as the *exact stamp*, from the cell's `title`.
///
/// Not the visible text: that reads "5 minutes ago" and drifts on its
/// own, so comparing it across a sync would be comparing two clocks
/// rather than two records. The title is the instant the row is
/// actually claiming, which is the thing these tests are about.
/// Null when the row has never run and there is no stamp to reveal.
async function lastSyncedOf(page: Page, id: string): Promise<string | null> {
  // Count first, like `statusOf`. A never-run row renders "—" with no
  // `title` at all, and calling `getAttribute` on a locator that
  // matches nothing *waits* for it rather than answering null — the
  // whole test times out instead of reporting "no stamp".
  const el = row(page, id).locator('[col-id="lastSynced"] [title]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("title");
}

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

async function writeConfig(page: Page, text: string) {
  await openManager(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();

  // Reload, so the rows this test then watches were painted from the
  // config *and* the runner's record together.
  //
  // Saving re-derives the table from the config text at once — that is
  // the point of the Advanced editor — but the per-step history behind
  // the Status and Last synced columns comes from `GET /api/dag`, which
  // is refetched on a timer. Between the two, a row that has run before
  // paints as "Never run": it exists because the config declares it,
  // and nothing has yet said what it did. Mounting the page afresh
  // fetches config, jobs and the DAG record in one `Promise.all`, so
  // that in-between state cannot be observed.
  //
  // Without this the sequence sampler below recorded
  // `["Queued", "Never run", "Succeeded"]` and failed the monotonicity
  // check — correctly, by its own rule that "Never run" after a queue
  // is going backwards. The status was a rendering artifact of the save
  // rather than anything the runner did.
  await openManager(page);
}

const TERMINAL = /^(Succeeded|Up to date|Failed|Blocked|Interrupted)$/;

/// Wait for the *row* to reach a terminal status, and return it.
///
/// Split out from `settle` because it is the half that does not
/// navigate: a caller reading a transition log recorded in the page
/// (see `recordStatuses`) has to get its answer before the reload that
/// `settle` ends with throws the log away.
///
/// `expect.poll` on the status string rather than a bespoke loop, so a
/// timeout reports the state it was stuck on ("expected 'Queued' to
/// match …") instead of just expiring.
async function settleRow(page: Page, id: string, timeout = 60_000): Promise<string> {
  // The value the poll matched, not a fresh read. Re-reading after the
  // poll succeeds is a race: the row can be claimed by the *next* job
  // between the two reads, and the function then returns "Queued" from
  // a call whose whole contract is to return a terminal status.
  let last = "(no status)";
  await expect
    .poll(
      async () => {
        last = (await statusOf(page, id)) ?? "(no status)";
        return last;
      },
      {
        timeout,
        intervals: [250],
        message: `${id} never reached a terminal status`,
      },
    )
    .toMatch(TERMINAL);
  return last;
}

/// Wait for the row *and* the run behind it, then show the settled
/// state, and return the status the row landed on.
async function settle(page: Page, id: string, timeout = 60_000): Promise<string> {
  const last = await settleRow(page, id, timeout);

  // A terminal row is not a finished run, and the difference is what
  // made three tests in this file flaky.
  //
  // The runner walks the whole graph to publish output versions, so it
  // is still writing records after the step you asked for went green —
  // and those writes move `Last synced` on rows this file then reads.
  // The failures all looked like time going wrong rather than like a
  // race: two stamps four seconds apart across one header click, a
  // stamp that "changed while the clock was merely ticking".
  //
  // `run.live` is the lock on the data root (see `DagRun`), so it is
  // the one signal that means *nobody is writing*. `page.request`
  // rather than the `request` fixture: it shares the page's context, so
  // it carries the same auth header and needs no plumbing at the call
  // sites.
  await expect
    .poll(
      async () => {
        const dag = await (await page.request.get("/api/dag")).json();
        return dag.run?.live === true;
      },
      {
        timeout,
        intervals: [200],
        message: `a runner still holds the data root after ${id} went ${last}`,
      },
    )
    .toBe(false);

  // ...and the page has to be showing that. The grid refetches when the
  // runner's record moves, but that is debounced (300 ms, see
  // `backend/http/src/watch.rs`), so immediately after the lock drops
  // the columns can still be a beat behind. Mounting afresh fetches
  // config, jobs and the DAG record in one `Promise.all`, which is also
  // what keeps a row that has run from painting as "Never run".
  await openManager(page);
  return last;
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

test.describe("a real sync, driven from the grid", () => {
  // Each test writes the config, runs a real `datalib-dag`, and waits
  // for it to settle. Playwright's 30 s default is not enough headroom
  // for that on a cold action cache.
  test.setTimeout(120_000);

  test.skip(
    !STEP_BIN || !PDF_DIR,
    "needs FW_E2E_DATALIB_STEP + FW_E2E_PDF_FIXTURE_DIR from run_e2e.sh",
  );

  // A step's `command` is split shell-style, so a binary path is
  // single-quoted: the runfiles path contains a space whenever the
  // checkout does, and an unquoted one is split into `/Users/thad/Imbue`
  // and the rest ("spawn …: Permission denied"). `input_path` is a TOML
  // value, not argv, so it needs no such treatment.
  //
  // The render step needs no params: it reads the scan root back out of
  // the raw store, so it always converts exactly the tree the download
  // step walked.

  // Carry the `[[applets]]` stanza forward from whatever was there.
  //
  // Replacing the file without it *removes* the unified_index applet,
  // and the gateway restarts applets on any config change that drops or
  // alters their entry — which tears down the resident qmd daemon and
  // re-arms a model load for whichever spec searches next
  // (`score-sort-order` and `search-qmd-routing` both sort after this
  // file). Applets are invisible to the scheduler, so keeping the
  // stanza changes nothing this spec asserts; dropping it was incidental
  // and made an unrelated spec slower.
  const applets = () => {
    const at = original.indexOf("[[applets]]");
    return at === -1 ? "" : `\n${original.slice(at)}`;
  };

  const config = () => `data_root = "${dataRoot}"

[[steps]]
id = "pdfs/raw"
command = "'${STEP_BIN}' download pdf"
[steps.params.common]
input_path = "${PDF_DIR}"

[[steps]]
id = "pdfs/rendered_md"
command = "'${STEP_BIN}' render pdf"
inputs = ["pdfs/raw"]

[[steps]]
id = "docs/raw"
command = "'${STEP_BIN}' download fsindex"
[steps.params.common]
input_path = "${dataRoot}/fsindex_scan"

# Declared and never synced by any test in this file, so "never run" is
# a state the grid can be observed handling — a Last synced of "—", and
# a row that has to stay at the bottom of that column whichever way it
# is sorted. Without a row like this the sort test passes with the
# comparator deleted, because same-offset ISO stamps happen to sort
# correctly as text.
[[steps]]
id = "unsynced/raw"
command = "'${STEP_BIN}' download fsindex"
[steps.params.common]
input_path = "${dataRoot}/fsindex_scan"
${applets()}`;

  test("syncing one source leaves another source's history untouched", async ({
    page,
  }) => {
    await writeConfig(page, config());

    // Give docs a real history to protect.
    await syncBtn(page, "docs/raw").click();
    expect(await settle(page, "docs/raw")).toBe("Succeeded");
    const docsStatus = await statusOf(page, "docs/raw");
    const docsSynced = await lastSyncedOf(page, "docs/raw");
    expect(docsSynced, "a synced row should carry an exact stamp").toBeTruthy();

    // Now sync the *other* source. The runner still walks docs/raw, to
    // publish its output version, and reports it `not_selected` — the
    // fact that used to be written over its record.
    await syncBtn(page, "pdfs/raw").click();
    expect(await settle(page, "pdfs/raw")).toBe("Succeeded");

    expect(
      await statusOf(page, "docs/raw"),
      "a sync of pdfs must not restate what docs did",
    ).toBe(docsStatus);
    expect(
      await lastSyncedOf(page, "docs/raw"),
      "nor when it did it — this timestamp used to move on every unrelated sync",
    ).toBe(docsSynced);
  });

  test("the row shows the sync happening, and never goes backwards", async ({
    page,
  }) => {
    await writeConfig(page, config());

    // Watch the row the way the grid paints it, from before the click
    // until it settles. This is the real sequence — the unit suite
    // replays a synthetic one through the same state machine.
    //
    // Recorded from mutations rather than sampled on a timer, which is
    // what this used to do (a `for(;;)` loop around
    // `waitForTimeout(150)`). Two things change as a result: the
    // sleep is gone, and the sequence is *complete* — see
    // `recordStatuses`.
    await recordStatuses(page, ["pdfs/raw", "pdfs/rendered_md"]);
    // Whatever the rows say before the click. The recorder seeds itself
    // with the current value, so this is 1 for a row with a status and
    // 0 for one still painting; everything past it is what the click
    // caused.
    const beforeUp = (await statusLog(page, "pdfs/raw")).length;
    const beforeDown = (await statusLog(page, "pdfs/rendered_md")).length;

    await syncBtn(page, "pdfs/raw").click();
    // Gate on the queue having accepted before *asserting*. `click()`
    // resolves when the event is dispatched, not when the async handler
    // behind it finishes, so an assertion straight after it races the
    // enqueue. The banner is set by `runSource` between the POST
    // returning and the job list being re-read, which is exactly the
    // moment this test is about: the queue has the job, and the
    // question is what the rows say. (The recorder was already running,
    // so nothing is missed while this resolves — that is the point of
    // starting it before the click.)
    await expect(page.getByText(/Queued a sync for/)).toBeVisible();

    // Syncing a source claims everything downstream of it, so the
    // render step is queued from the same first frame — before the
    // runner exists, let alone reaches it. This is the assertion a
    // download-only provider could not support, and the reason this
    // spec is built on `pdf`.
    const downstream = (await statusLog(page, "pdfs/rendered_md")).slice(beforeDown);
    expect(downstream[0], `downstream sequence was ${JSON.stringify(downstream)}`).toBe(
      "Queued",
    );
    // ...while the unrelated source is not claimed at all.
    expect(await statusOf(page, "docs/raw")).not.toBe("Queued");

    // `settleRow`, not `settle`: the log lives in the page, and
    // `settle` ends with a reload that would throw it away.
    await settleRow(page, "pdfs/raw");
    const seen = (await statusLog(page, "pdfs/raw")).slice(beforeUp);

    // What the sequence must contain. "Queued" is the frame that used
    // to be missing entirely — the click produced no visible change
    // until the whole run was over.
    expect(seen[0], `sequence was ${JSON.stringify(seen)}`).toBe("Queued");
    expect(seen[seen.length - 1]).toBe("Succeeded");

    // "Running" stays optional, and recording the transitions is what
    // settled *why*.
    //
    // The sampler this replaces guessed: "a scan of a small tree can
    // finish inside one sample". It could not tell a status that never
    // appeared from one it blinked past, so it had to allow both. The
    // recorder can, and the answer is the first: on this fixture the
    // sequence is `["Queued","Succeeded"]` — the row never paints
    // Running at all.
    //
    // That is not a sampling artifact and not a bug. The board the
    // status comes from is published by the worker at ~400 ms
    // (`backend/http/src/worker.rs`), and a PDF scan of two files
    // starts and finishes well inside one of those windows — so no
    // published frame ever carries the step as `running`, and there is
    // nothing for any observer to see. Asserting it would be asserting
    // that this fixture is slow.
    //
    // What the complete sequence *does* buy is the check below: a
    // backwards transition that lasted less than a sample used to be
    // invisible, and now is not.

    // The sequence must be monotonic. A status going backwards reads as
    // "about to run again", which is worse than a stale one.
    // Total over the whole vocabulary, on purpose. An unranked status
    // used to make this crash with "received value must be a number"
    // and no hint as to which status it choked on — a test that fails
    // uninformatively about the one thing it exists to describe.
    //
    // "Never run" ranks *below* Queued: it is the absence of history,
    // so seeing it after a sync was queued really is going backwards.
    const rank: Record<string, number> = {
      "Never run": -1,
      Queued: 0,
      Running: 1,
      Succeeded: 2,
      "Up to date": 2,
      Failed: 2,
      Blocked: 2,
      Interrupted: 2,
    };
    const rankOf = (s: string) => {
      expect(
        rank[s],
        `unranked status ${JSON.stringify(s)} in ${JSON.stringify(seen)}`,
      ).toBeDefined();
      return rank[s];
    };
    for (let i = 1; i < seen.length; i++) {
      expect(
        rankOf(seen[i]),
        `went backwards: ${JSON.stringify(seen)}`,
      ).toBeGreaterThanOrEqual(rankOf(seen[i - 1]));
    }

    // The render step follows the download it depends on: it may not
    // reach a terminal state before its input does. `pdfs/raw` is
    // already terminal here, so waiting on the render is bounded.
    //
    // `settleRow` again, and the log re-read afterwards: the recorder
    // has been running throughout, so this is the whole downstream
    // sequence rather than the two ends of it plus whatever a sampler
    // happened to catch in between.
    expect(await settleRow(page, "pdfs/rendered_md")).toMatch(/^(Succeeded|Up to date)$/);
    const downstreamFinal = (await statusLog(page, "pdfs/rendered_md")).slice(beforeDown);
    expect(
      downstreamFinal[0],
      `downstream never started Queued: ${JSON.stringify(downstreamFinal)}`,
    ).toBe("Queued");
    expect(
      downstreamFinal[downstreamFinal.length - 1],
      `downstream never finished: ${JSON.stringify(downstreamFinal)}`,
    ).toMatch(/^(Succeeded|Up to date)$/);

    // The run itself has to be over before the next test writes a
    // config into this root — the half of `settle` that `settleRow`
    // leaves out. Cheap here: everything is already terminal.
    await settle(page, "pdfs/rendered_md");
  });

  test("Last synced counts up on its own, and hover reveals the exact stamp", async ({
    page,
  }) => {
    // What only a browser can answer about this column. The arithmetic
    // — every unit boundary, a stamp in another UTC offset, one in the
    // future — is in src/config/timeFormat.test.ts, because provoking
    // "6 days ago" from a live backend would mean forging the runner's
    // state file. What that unit test cannot show is any of the below:
    // that the column is wired to the relative form at all, that the
    // absolute stamp survives as the hover, and that the cell keeps
    // counting with nobody touching the page.
    await writeConfig(page, config());
    await syncBtn(page, "pdfs/raw").click();
    expect(await settle(page, "pdfs/raw")).toBe("Succeeded");

    const cell = row(page, "pdfs/raw").locator('[col-id="lastSynced"]');
    await expect(cell).toHaveText(/(just now|\d+ seconds? ago)/);

    // The exact instant is still reachable, on the hover.
    const stamp = await lastSyncedOf(page, "pdfs/raw");
    expect(stamp, "the relative text must not be the only record").toBeTruthy();
    expect(stamp).toMatch(/\d{2}:\d{2}:\d{2}/);

    // ...and it counts up without anything else happening. This is the
    // half that needs a real clock and a real repaint loop: the value
    // is derived from `Date.now()` at paint time, so a column that
    // never repaints would sit at "1 second ago" forever and every
    // other assertion here would still pass.
    const before = (await cell.textContent())?.trim() ?? "";
    await expect
      .poll(async () => (await cell.textContent())?.trim(), {
        timeout: 15_000,
        intervals: [500],
        message: `Last synced never advanced past "${before}"`,
      })
      .not.toBe(before);

    // The stamp it advanced *against* is unchanged — the row is not
    // re-syncing, the clock is just moving.
    expect(await lastSyncedOf(page, "pdfs/raw")).toBe(stamp);

    // A row that never ran has no time to be relative to, and nothing
    // to reveal. `unsynced/raw` exists in the config for exactly this:
    // the data root is shared by every test in this file, so any step
    // one of them syncs would make this order-dependent.
    await expect(
      row(page, "unsynced/raw").locator('[col-id="lastSynced"]'),
    ).toHaveText("—");
    expect(await lastSyncedOf(page, "unsynced/raw")).toBeNull();
  });

  test("sorting Last synced orders by time, not by how the cell reads", async ({
    page,
  }) => {
    // The column shows "5 minutes ago" and sorts on the underlying
    // stamp. Those two orders genuinely disagree here, which is what
    // makes this worth asserting through the real header rather than
    // only against the comparator: alphabetically "1 hour ago" precedes
    // "4 seconds ago", while chronologically it follows it.
    await writeConfig(page, config());

    // Two rows with a real gap between them, so the orders differ.
    await syncBtn(page, "docs/raw").click();
    expect(await settle(page, "docs/raw")).toBe("Succeeded");
    await syncBtn(page, "pdfs/raw").click();
    expect(await settle(page, "pdfs/raw")).toBe("Succeeded");
    expect(await settle(page, "pdfs/rendered_md")).toMatch(/^(Succeeded|Up to date)$/);

    /// Row ids top to bottom, paired with the exact stamp each is
    /// claiming — read off `title`, so the check is against instants
    /// rather than against the prose the cell happens to render.
    const ordering = async () => {
      const ids = await page
        .locator('.ag-center-cols-container .ag-row')
        .evaluateAll((rows) =>
          rows
            .sort(
              (a, b) =>
                Number((a as HTMLElement).getAttribute("aria-rowindex")) -
                Number((b as HTMLElement).getAttribute("aria-rowindex")),
            )
            .map((r) => ({
              id: r.getAttribute("row-id") ?? "",
              stamp:
                r.querySelector('[col-id="lastSynced"] [title]')?.getAttribute("title") ??
                null,
            })),
        );
      return ids;
    };

    const header = page.locator('.ag-header-cell[col-id="lastSynced"]');

    await header.click(); // ascending — oldest first
    const asc = await ordering();
    const ascTimes = asc.map((r) => r.stamp).filter((s): s is string => !!s);
    expect(ascTimes.length, "need at least two stamped rows to order").toBeGreaterThan(1);
    for (let i = 1; i < ascTimes.length; i++) {
      expect(
        Date.parse(ascTimes[i]),
        `ascending is out of order at ${i}: ${JSON.stringify(asc)}`,
      ).toBeGreaterThanOrEqual(Date.parse(ascTimes[i - 1]));
    }

    await header.click(); // descending — newest first
    const desc = await ordering();
    const descTimes = desc.map((r) => r.stamp).filter((s): s is string => !!s);
    for (let i = 1; i < descTimes.length; i++) {
      expect(
        Date.parse(descTimes[i]),
        `descending is out of order at ${i}: ${JSON.stringify(desc)}`,
      ).toBeLessThanOrEqual(Date.parse(descTimes[i - 1]));
    }

    // ...and it really did reverse, rather than the click doing nothing.
    expect(descTimes).toEqual([...ascTimes].reverse());

    // A row that never ran sorts as "forever ago" — older than anything
    // that has run. So it leads ascending and trails descending, and
    // the reversal above covers the whole column rather than stopping
    // short of the nulls. Sorting by this header once is how you ask
    // "what has never run?".
    const unrun = (rows: { id: string; stamp: string | null }[]) =>
      rows.filter((r) => r.stamp === null).map((r) => r.id);
    expect(unrun(asc), "the un-synced row should be in the table").toContain(
      "unsynced/raw",
    );
    expect(
      asc.slice(0, unrun(asc).length).every((r) => r.stamp === null),
      `ascending: never-run rows should lead — ${JSON.stringify(asc)}`,
    ).toBe(true);
    expect(
      desc.slice(-unrun(desc).length).every((r) => r.stamp === null),
      `descending: never-run rows should trail — ${JSON.stringify(desc)}`,
    ).toBe(true);

    // The whole column reversed, nulls included — not just its stamped
    // middle. This is the property that makes the order a single total
    // one rather than two rules stitched together.
    //
    // Worth knowing what this test does NOT pin: deleting
    // `compareStamps` entirely leaves the whole suite green (measured).
    // Same-offset ISO stamps sort correctly as text by accident, and
    // "forever ago" is what AG Grid's default already does with nulls,
    // so nothing observable here distinguishes the two. The comparator
    // earns its place on stamps in *different* UTC offsets, which one
    // machine cannot produce — that case lives in
    // src/config/timeFormat.test.ts and is the real coverage.
    //
    // Compared on stamps, not row ids: `pdfs/raw` and its render step
    // finish inside the same second, so they compare equal, and a
    // stable sort leaves tied rows in the order it found them rather
    // than swapping them on reversal. Their stamps are equal too, so
    // the stamp sequence reverses cleanly whichever way the tie fell.
    expect(
      desc.map((r) => r.stamp),
      "descending should be ascending reversed, end to end",
    ).toEqual([...asc].reverse().map((r) => r.stamp));
  });

  test("a downstream step can't be synced on its own, and says what would carry it", async ({
    page,
  }) => {
    await writeConfig(page, config());

    // `datalib-dag` rejects a `--sync` naming anything but a source
    // step, so this button would only ever queue a job that fails on
    // startup. It is disabled, and names the row that does carry it.
    const btn = syncBtn(page, "pdfs/rendered_md");
    await expect(btn).toBeDisabled();
    await expect(btn).toHaveAttribute("title", /Run pdfs\/raw/);

    // A source step, by contrast, is runnable.
    await expect(syncBtn(page, "pdfs/raw")).toBeEnabled();
  });
});
