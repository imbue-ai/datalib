// Driving a real sync from the Pipeline grid, and watching the whole
// sequence it produces.
//
//   * **A sync of one source must not touch another's history.** Every
//     run walks the whole graph to publish output versions, and it used
//     to write `not_selected` into the steps it walked past — so a
//     source that succeeded yesterday came back as "not selected",
//     stamped with the time of a run that never touched it.
//   * **The row must show the sync happening.** "Running" only reached
//     the runner's record when a step *finished*, so pressing Sync looked
//     like nothing had happened until it was over.
//
// `pdf` is the local-only provider that has *both* halves, which is why
// it carries most of this spec: an `ingest -> render_markdown` edge is
// what makes "everything downstream is queued too" a real assertion
// about the DAG rather than a contrived one. `fsindex` (download-only)
// is the unrelated second source — the one whose history must not move.
// Watching a run in flight needs a run that cannot finish before it is
// seen, so that test syncs a replayed ChatGPT download instead, held
// while it watches.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { rmSync, writeFileSync } from "node:fs";
import {
  expandGroup,
  groupRow,
  pickRowMenu,
  lastSuccessOf,
  pipelineRow as row,
  recordStatuses,
  settle,
  settleRow,
  settleRunner,
  stampOf as lastSyncedOf,
  untilTheSecondTurns,
  stampsBefore,
  statusLog,
  statusWord,
  statusOf,
  TERMINAL,
  TABLE_ROWS,
  MANAGE_WITH_CONFIG,
} from "./grid-helpers";
import { expectSanePaints, watchPaints } from "./paint-watch";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.DATALIB_TEST_E2E_DATALIB_STEP;
const PDF_DIR = process.env.DATALIB_TEST_E2E_PDF_FIXTURE_DIR;
const PLAYBACK = process.env.DATALIB_TEST_E2E_PLAYBACK_DIR;
const HOLD = process.env.DATALIB_TEST_E2E_SYNC_PLAYBACK_HOLD;

/// Park every replayed request of this spec's backend until `release`.
function hold() {
  writeFileSync(HOLD!, "");
}
/// Let a held download run to its end at fixture speed.
function release() {
  if (HOLD) rmSync(HOLD, { force: true });
}

/// A source that replays a tape, so it can be held in flight.
const TAPED = "chatgpt-replay";
const TAPED_UP = `${TAPED}/ingest`;
const TAPED_DOWN = `${TAPED}/render_markdown`;
const TAPED_STANZA = `
[[groups]]
id = "${TAPED}"
type = "chatgpt"

[[steps]]
group = "${TAPED}"
function = "ingest"
command = "'${STEP_BIN}'"
[steps.params.api]

[[steps]]
group = "${TAPED}"
function = "render_markdown"
command = "'${STEP_BIN}'"
inputs = ["${TAPED_UP}"]
`;

/// This spec's own data root, asked of the backend rather than read
/// from the environment.
let dataRoot = "";
async function resolveDataRoot(request: APIRequestContext): Promise<string> {
  const { path } = (await (await request.get("/api/config")).json()) as {
    path: string;
  };
  return path.slice(0, path.lastIndexOf("/"));
}

const syncBtn = (page: Page, id: string) => row(page, id).getByRole("button", { name: "Sync now" });

async function openManager(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
}

async function writeConfig(page: Page, text: string) {
  await openManager(page);
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();

  // Reload, so the rows this test then watches were painted from the
  // config *and* the runner's record together.
  //
  // Without this the sequence sampler below recorded
  // `["Queued", "Never run", "Succeeded"]` and failed the monotonicity
  // check — correctly, by its own rule that "Never run" after a queue
  // is going backwards. The status was a rendering artifact of the save
  // rather than anything the runner did.
  //
  // Saving re-derives the table from the config text at once — that is
  // the point of the Advanced editor — but the per-step history behind
  // the Status and Last synced columns comes from `GET /api/dag`, which
  // is refetched separately. Between the two, a row that has run before
  // paints as "Never run": it exists because the config declares it,
  // and nothing has yet said what it did. Mounting the page afresh
  // fetches the config and the rows together, so that in-between state
  // cannot be observed.
  await openManager(page);
}

/// The step rows this file drives live under groups, and a group's
/// steps have rows only while it is open. Opened once per test; the
/// grid remembers across the remounts `settle` does.
async function writeConfigAndOpenGroups(page: Page, text: string) {
  await writeConfig(page, text);
  for (const id of ["pdfs", "docs", "unsynced"]) await expandGroup(page, id);
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

test.describe("a real sync, driven from the grid", () => {
  // Each test writes the config, runs a real `datalib-dag`, and waits
  // for it to settle. Playwright's 30 s default is not enough headroom
  // for that on a cold action cache.
  test.setTimeout(120_000);

  test.skip(
    !STEP_BIN || !PDF_DIR,
    "needs DATALIB_TEST_E2E_DATALIB_STEP + DATALIB_TEST_E2E_PDF_FIXTURE_DIR from run_e2e.sh",
  );

  // A step's `command` is split shell-style, so a binary path is
  // single-quoted: the runfiles path contains a space whenever the
  // checkout does, and an unquoted one is split into `/Users/thad/Imbue`
  // and the rest ("spawn …: Permission denied"). `fswalk.path` is a TOML
  // value, not argv, so it needs no such treatment.
  //
  // The render step needs no params: it reads the scan root back out of
  // the raw store, so it always converts exactly the tree the download
  // step walked.

  // Carry the `[[applets]]` stanza forward from whatever was there.
  const applets = () => {
    const at = original.indexOf("[[applets]]");
    return at === -1 ? "" : `\n${original.slice(at)}`;
  };

  const config = () => `data_root = "${dataRoot}"

[[groups]]
id = "pdfs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"
command = "'${STEP_BIN}'"
[steps.params.fswalk]
path = "${PDF_DIR}"

[[steps]]
group = "pdfs"
function = "render_markdown"
command = "'${STEP_BIN}'"
inputs = ["pdfs/ingest"]

[[groups]]
id = "docs"
type = "fsindex"

[[steps]]
group = "docs"
function = "ingest"
command = "'${STEP_BIN}'"
[steps.params.fswalk]
path = "${dataRoot}/fsindex_scan"

# Declared and never synced by any test in this file, so "never run" is
# a state the grid can be observed handling — a Last synced of "—", and
# a row that has to stay at the bottom of that column whichever way it
# is sorted. Without a row like this the sort test passes with the
# comparator deleted, because same-offset ISO stamps happen to sort
# correctly as text.
[[groups]]
id = "unsynced"
type = "fsindex"

[[steps]]
group = "unsynced"
function = "ingest"
command = "'${STEP_BIN}'"
[steps.params.fswalk]
path = "${dataRoot}/fsindex_scan"
${applets()}`;

  test("syncing one source leaves another source's history untouched", async ({ page }) => {
    await writeConfigAndOpenGroups(page, config());

    // Give docs a real history to protect.
    const docsWas = await lastSyncedOf(page, "docs/ingest");
    await syncBtn(page, "docs/ingest").click();
    expect(await settle(page, "docs/ingest", docsWas)).toBe("Succeeded");
    const docsStatus = await statusOf(page, "docs/ingest");
    const docsSynced = await lastSyncedOf(page, "docs/ingest");
    expect(docsSynced, "a synced row should carry an exact stamp").toBeTruthy();

    // Now sync the *other* source. A run that walked docs/raw used to
    // write "not selected" over its record.
    const pdfsWas = await lastSyncedOf(page, "pdfs/ingest");
    await syncBtn(page, "pdfs/ingest").click();
    expect(await settle(page, "pdfs/ingest", pdfsWas)).toBe("Succeeded");

    expect(
      await statusOf(page, "docs/ingest"),
      "a sync of pdfs must not restate what docs did",
    ).toBe(docsStatus);
    expect(
      await lastSyncedOf(page, "docs/ingest"),
      "nor when it did it — this timestamp used to move on every unrelated sync",
    ).toBe(docsSynced);
  });

  test("the row shows the sync happening, and never goes backwards", async ({ page }) => {
    test.skip(
      !PLAYBACK || !HOLD,
      "needs DATALIB_TEST_E2E_PLAYBACK_DIR + DATALIB_TEST_E2E_SYNC_PLAYBACK_HOLD from run_e2e.sh",
    );
    // A replayed download rather than `pdfs`: on this root a re-walk of
    // the PDF folder finds nothing new and goes Queued → Running →
    // Succeeded between two repaints, so the in-flight frame this test
    // is about was there to see only when the runner was slow. The tape
    // is held from before the click until that frame has been seen,
    // then let go.
    await writeConfig(page, `${config()}${TAPED_STANZA}`);
    for (const id of [TAPED, "docs"]) await expandGroup(page, id);

    // Watch the row the way the grid paints it, from before the click
    // until it settles. This is the real sequence — the unit suite
    // replays a synthetic one through the same state machine.
    await recordStatuses(page, [TAPED_UP, TAPED_DOWN]);
    // Whatever the rows say before the click. The recorder seeds itself
    // with the current value, so this is 1 for a row with a status and
    // 0 for one still painting; everything past it is what the click
    // caused.
    const beforeUp = (await statusLog(page, TAPED_UP)).length;
    const beforeDown = (await statusLog(page, TAPED_DOWN)).length;
    const since = async (id: string, before: number) => (await statusLog(page, id)).slice(before);

    const was = await stampsBefore(page, [TAPED_UP, TAPED_DOWN]);
    // From the click to the run's end the rows move under a still
    // pointer; they should move in place.
    const paints = await watchPaints(page.locator(".tg-grid").first());
    hold();
    await syncBtn(page, TAPED_UP).click();
    // `click()` resolves when the event is dispatched, not when the
    // POST behind it returns; the banner is set once it has.
    await expect(page.getByText(/Queued a sync for/)).toBeVisible();

    // The download cannot finish while the tape is held, so its row
    // reaching Running is a state to wait for, not a frame to catch.
    await expect
      .poll(async () => (await since(TAPED_UP, beforeUp)).map(statusWord), {
        timeout: 30_000,
        intervals: [100],
        message: `${TAPED_UP} never showed the run in flight`,
      })
      .toContain("Running");

    // Syncing a source takes on everything downstream of it, so the
    // render is in flight too: Queued behind its download, or Running on
    // what the download has published, since the download streams.
    // This is the assertion a download-only source could not support.
    await expect
      .poll(async () => (await since(TAPED_DOWN, beforeDown)).length, {
        timeout: 10_000,
        intervals: [50],
        message: "the render row never repainted after the click",
      })
      .toBeGreaterThan(0);
    const downstream = await since(TAPED_DOWN, beforeDown);
    expect(
      statusWord(downstream[0]),
      `downstream sequence was ${JSON.stringify(downstream)}`,
    ).toMatch(/^(Queued|Running)$/);
    // ...while the unrelated source is not claimed at all.
    expect(await statusOf(page, "docs/ingest")).not.toBe("Queued");
    // Only the hold keeps it here; if the step has finished anyway, the
    // frames above were caught by luck and the next slow run will miss them.
    expect(await statusOf(page, TAPED_UP), "the download finished while its tape was held").toBe(
      "Running",
    );

    release();
    // `settleRow`, not `settle`: the log lives in the page, and
    // `settle` remounts, which would throw it away. The before-stamp is
    // still passed — a terminal status on its own is answerable by the
    // *previous* run's frame, which is what #237 fixed.
    await settleRow(page, TAPED_UP, was[TAPED_UP]);
    const seen = await since(TAPED_UP, beforeUp);
    expect(
      seen.length,
      `the row never showed the run in flight: sequence was ${JSON.stringify(seen)}`,
    ).toBeGreaterThan(0);

    // What the sequence must contain: a frame from before the run was
    // over, which used to be missing entirely — the click produced no
    // visible change until the whole run was done.
    expect(statusWord(seen[0]), `sequence was ${JSON.stringify(seen)}`).toMatch(
      /^(Queued|Running)$/,
    );
    expect(statusWord(seen[seen.length - 1]), `sequence was ${JSON.stringify(seen)}`).toBe(
      "Succeeded",
    );

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
      Stopped: 2,
    };
    const rankOf = (frame: string) => {
      const s = statusWord(frame);
      expect(
        rank[s],
        `unranked status ${JSON.stringify(s)} in ${JSON.stringify(seen)}`,
      ).toBeDefined();
      return rank[s];
    };
    for (let i = 1; i < seen.length; i++) {
      expect(rankOf(seen[i]), `went backwards: ${JSON.stringify(seen)}`).toBeGreaterThanOrEqual(
        rankOf(seen[i - 1]),
      );
    }

    // The render step follows the download it depends on: it may not
    // reach a terminal state before its input does. The download is
    // already terminal here, so waiting on the render is bounded.
    expect(await settleRow(page, TAPED_DOWN, was[TAPED_DOWN])).toMatch(/^(Succeeded|Up to date)$/);
    const downstreamFinal = await since(TAPED_DOWN, beforeDown);
    expect(
      statusWord(downstreamFinal[downstreamFinal.length - 1]),
      `downstream never finished: ${JSON.stringify(downstreamFinal)}`,
    ).toMatch(/^(Succeeded|Up to date)$/);
    expectSanePaints(await paints(), "the Pipeline table, through a sync");

    // The run itself has to be over before the next test writes a
    // config into this root — the half of `settleRows` that `settleRow`
    // leaves out. Cheap here: every row is already terminal.
    await settleRunner(page);
  });

  test("Last synced holds still under a minute, then crosses to 1 minute ago", async ({ page }) => {
    // What only a browser can answer about this column. The arithmetic
    // — every unit boundary, a stamp in another UTC offset, one in the
    // future — is in src/config/timeFormat.test.ts, because provoking
    // "6 days ago" from a live backend would mean forging the runner's
    // state file. What that unit test cannot show is any of the below:
    // that the column is wired to the relative form at all, that the
    // absolute stamp survives as the hover, and what the repaint loop
    // does to a cell nobody has touched.
    //
    // That loop is `setInterval(tickRelative, 1000)` in TableGrid.ce.vue,
    // and it is driven here by a fake clock rather than by waiting —
    // which is what lets this test assert the minute crossing at all.
    // Installed before this test's first navigation, as the clock API
    // requires, and left ticking so the real sync below still runs
    // against a clock that moves on its own.
    await page.clock.install();

    await writeConfigAndOpenGroups(page, config());
    const countUpWas = await lastSyncedOf(page, "pdfs/ingest");
    await syncBtn(page, "pdfs/ingest").click();
    expect(await settle(page, "pdfs/ingest", countUpWas)).toBe("Succeeded");

    const cell = row(page, "pdfs/ingest").locator('[col-id="last_synced"]');
    await expect(cell).toHaveText("seconds ago");

    // The exact instant is still reachable, on the hover.
    const stamp = await lastSyncedOf(page, "pdfs/ingest");
    expect(stamp, "the relative text must not be the only record").toBeTruthy();
    expect(stamp).toMatch(/\d{2}:\d{2}:\d{2}/);

    // Cut the API off before touching the clock. Advancing time fires
    // every timer that comes due, including the rows poller, and each
    // of those calls `repaint()` on the way back — which refreshes this
    // cell for reasons that have nothing to do with the column's own
    // clock. With the fetches failing, `commitRows` never runs, so
    // `tickRelative` is the only thing left
    // that can repaint. Verified: without this the test passes with
    // `setInterval(tickRelative, …)` deleted outright.
    await page.route("**/api/**", (route) => route.abort());

    // Freeze, so that from here the only thing moving is the clock.
    // `pauseAt` refuses to move the clock backwards and the page's own
    // clock runs on while this round-trips, so the target is a moment
    // ahead of the reading rather than exactly on it. That second is
    // part of the margin accounted for below; nothing waits for it.
    const frozenAt = (await page.evaluate(() => Date.now())) + 1_000;
    await page.clock.pauseAt(frozenAt);

    // ...and it stays put. `runFor` fires every timer due in the
    // window, so this is four real turns of the repaint loop with
    // nothing else happening. Against the per-second countup this
    // column used to do, they would have read 3, 4, 5 — this is the
    // assertion that fails if the countup ever comes back.
    await page.clock.runFor(4000);
    await expect(cell, "Last synced ticked while nothing happened").toHaveText("seconds ago");

    // The crossing to "1 minute ago" — the only self-repaint this
    // column does, and the reason the loop exists. It went untested
    // while this spec waited on the wall clock, because catching it
    // meant spending a real minute in a suite that runs in about 40s.
    //
    // The stamp is a real instant from the run above, so the delta at
    // this point is 60s, plus the second skipped at the pause, plus
    // however long the assertions took. The reading holds to 90s
    // (`formatRelative` rounds), which is the margin.
    await page.clock.fastForward("01:00");
    await expect(cell).toHaveText("1 minute ago");

    // The stamp underneath is unchanged — the repaint moved the
    // relative text and left the instant alone, and the row is not
    // re-syncing.
    expect(await lastSyncedOf(page, "pdfs/ingest")).toBe(stamp);

    // A row that never ran has no time to be relative to, and nothing
    // to reveal. `unsynced/ingest` exists in the config for exactly this:
    // the data root is shared by every test in this file, so any step
    // one of them syncs would make this order-dependent.
    await expect(row(page, "unsynced/ingest").locator('[col-id="last_synced"]')).toHaveText("—");
    expect(await lastSyncedOf(page, "unsynced/ingest")).toBeNull();

    // The afterEach writes the config back through this same page: it
    // needs both the API and a clock that moves.
    await page.unroute("**/api/**");
    await page.clock.resume();
  });

  test("sorting Last synced orders by time, not by how the cell reads", async ({ page }) => {
    // The column shows "5 minutes ago" and sorts on the underlying
    // stamp. Those two orders genuinely disagree here, which is what
    // makes this worth asserting through the real header rather than
    // only against the comparator: alphabetically "1 hour ago" precedes
    // "seconds ago", while chronologically it follows it.
    await writeConfigAndOpenGroups(page, config());

    // Two rows with a real gap between them, so the orders differ. Each
    // sync must be finished before the next begins, or the stamps can
    // land in either order — which is the thing being sorted.
    const sortWas = await stampsBefore(page, [
      "docs/ingest",
      "pdfs/ingest",
      "pdfs/render_markdown",
    ]);
    await syncBtn(page, "docs/ingest").click();
    expect(await settle(page, "docs/ingest", sortWas["docs/ingest"])).toBe("Succeeded");
    await syncBtn(page, "pdfs/ingest").click();
    expect(await settle(page, "pdfs/ingest", sortWas["pdfs/ingest"])).toBe("Succeeded");
    expect(await settle(page, "pdfs/render_markdown", sortWas["pdfs/render_markdown"])).toMatch(
      /^(Succeeded|Up to date)$/,
    );

    /// Rows top to bottom, each with the exact stamp it claims — read
    /// off `title`, so the check is against instants rather than the
    /// prose the cell renders — and its depth in the tree, off the
    /// `slick-tree-level-N` class on the tree cell.
    type Seen = { id: string; level: number; stamp: string | null };
    const ordering = async (): Promise<Seen[]> =>
      page.locator(TABLE_ROWS).evaluateAll((rows) =>
        rows
          .sort(
            (a, b) =>
              Number((a as HTMLElement).getAttribute("data-row")) -
              Number((b as HTMLElement).getAttribute("data-row")),
          )
          .map((r) => ({
            id: r.getAttribute("data-key") ?? "",
            level: Number(
              /slick-tree-level-(\d+)/.exec(r.querySelector(".tg-tree")?.className ?? "")?.[1] ??
                "0",
            ),
            stamp: r.querySelector('[col-id="last_synced"] [title]')?.getAttribute("title") ?? null,
          })),
      );

    /// The sets a tree sort actually orders: the top-level rows, and
    /// each open group's children, keyed by the group. A sort is total
    /// among siblings and nowhere else — a group row shows its ingest
    /// step's stamp while its render child finished a second later, so
    /// the flattened list puts a newer child under an older parent when
    /// sorted descending, and no choice of the group's stamp fixes both
    /// directions at once (the newest child's would break ascending).
    const siblingSets = (rows: Seen[]): Map<string, Seen[]> => {
      const sets = new Map<string, Seen[]>([["top", []]]);
      let parent = "top";
      for (const r of rows) {
        if (r.level === 0) {
          sets.get("top")!.push(r);
          parent = r.id;
          sets.set(parent, []);
        } else {
          sets.get(parent)!.push(r);
        }
      }
      return sets;
    };

    /// Stamps monotone in `dir`, and never-run rows — "forever ago",
    /// older than anything that has run — at the old end: leading
    /// ascending, trailing descending. One click on the header is how
    /// you ask "what has never run?".
    const expectOrdered = (name: string, set: Seen[], dir: "asc" | "desc") => {
      const stamps = set.map((r) => r.stamp);
      const nulls = stamps.filter((s) => s === null).length;
      const times = stamps.filter((s): s is string => !!s).map((s) => Date.parse(s));
      for (let i = 1; i < times.length; i++) {
        const ok = dir === "asc" ? times[i] >= times[i - 1] : times[i] <= times[i - 1];
        expect(ok, `${dir} ${name} is out of order at ${i}: ${JSON.stringify(set)}`).toBe(true);
      }
      const nullEnd = dir === "asc" ? stamps.slice(0, nulls) : stamps.slice(stamps.length - nulls);
      expect(
        nullEnd.every((s) => s === null),
        `${dir} ${name}: never-run rows should ${dir === "asc" ? "lead" : "trail"} — ${JSON.stringify(set)}`,
      ).toBe(true);
    };

    const header = page.locator('.tg-grid .slick-header-column[col-id="last_synced"]');

    await header.click(); // ascending — oldest first
    const asc = siblingSets(await ordering());
    // The sets this config gives something to order: two synced groups
    // beside a never-synced one at the top, and pdfs' two steps under
    // it, whose stamps differ by however long the render took.
    const stamped = (set: Seen[]) => set.filter((r) => r.stamp !== null).length;
    expect(stamped(asc.get("top")!), "need two stamped groups to order").toBeGreaterThan(1);
    expect(stamped(asc.get("group:pdfs")!), "need pdfs' two steps to order").toBe(2);
    expect(
      asc.get("group:unsynced")!.map((r) => r.id),
      "the un-synced row should be in the table",
    ).toEqual(["unsynced/ingest"]);
    for (const [name, set] of asc) expectOrdered(name, set, "asc");

    await header.click(); // descending — newest first
    const desc = siblingSets(await ordering());
    for (const [name, set] of desc) expectOrdered(name, set, "desc");

    // ...and it really did reverse, rather than the click doing
    // nothing: every sibling set, nulls included — not just its stamped
    // middle. This is the property that makes the order a single total
    // one rather than two rules stitched together.
    expect([...desc.keys()].sort()).toEqual([...asc.keys()].sort());
    for (const [name, set] of asc) {
      expect(
        desc.get(name)!.map((r) => r.stamp),
        `${name}: descending should be ascending reversed, end to end`,
      ).toEqual([...set].reverse().map((r) => r.stamp));
    }
  });

  // A step that ends badly: its hover does not quote the error — that
  // is a log line, and the log is where it reads with what led up to
  // it — and the double-click the hover promises opens the log there.
  test("a failed step's hover points at the log, and the double-click opens it at the error", async ({
    page,
  }) => {
    const flaky = `${config()}

[[groups]]
id = "flaky"
type = "pdf"

[[steps]]
group = "flaky"
function = "ingest"
command = "/bin/sh -c 'echo walking page 1 >&2; echo listing failed: 429 too many requests >&2; exit 1'"
`;
    await writeConfig(page, flaky);
    await expandGroup(page, "flaky");

    const was = await lastSyncedOf(page, "flaky/ingest");
    await syncBtn(page, "flaky/ingest").click();
    expect(await settle(page, "flaky/ingest", was)).toBe("Failed");
    await expandGroup(page, "flaky");

    const cell = row(page, "flaky/ingest").locator('[col-id="status"] .tg-status');
    await expect(cell).toHaveAttribute(
      "title",
      "Failed — double-click to open the log at the error",
    );
    // The group reads its failed child's status, and names it.
    await expect(
      page.locator(`${TABLE_ROWS}[data-key="group:flaky"] [col-id="status"] .tg-status`),
    ).toHaveAttribute("title", "Failed — flaky/ingest: double-click to open the log at the error");

    await cell.dblclick();
    // The log is the column after the Manage card.
    const dialog = page.locator(".miller-col").filter({ has: page.locator(".rl-panel") });
    await expect(dialog).toBeVisible();
    await expect(dialog.locator(".miller-col-title")).toHaveText("Log · flaky/ingest");
    // Opened on the step's attempt — a process of the run, with how it
    // ended in its name — and on the whole of it: the step's own words
    // and the runner's about it.
    const which = dialog.getByLabel("Which process of the run");
    await expect(which.locator("option:checked")).toHaveText(
      /^flaky\/ingest · attempt 1 · exited 1$/,
    );
    // The line the panel opened on is the runner's word on how the step
    // ended, marked, with the step's own last words above it.
    const jumped = dialog.locator('.rl-grid .slick-cell.rl-jumped[col-id="msg"]');
    await expect(jumped).toBeVisible({ timeout: 10_000 });
    await expect(jumped).toHaveText(/^step flaky\/ingest exited with exit status: 1: /);
    await expect(jumped).toContainText("listing failed: 429 too many requests");
    await expect(jumped).toHaveClass(/rl-error/);
    const messages = dialog.locator(
      '.rl-grid .slick-row:not(.slick-group) .slick-cell[col-id="msg"]',
    );
    await expect(messages.filter({ hasText: /^walking page 1$/ })).toBeVisible();
  });

  // A source that syncs once and fails from then on: Last synced
  // follows the failure, Last success stays on the one that worked —
  // on the step and on its group (#646).
  test("a failure after a success moves Last synced and leaves Last success", async ({ page }) => {
    // Named per attempt, so a retry does not find the last one's marker
    // and fail its first sync.
    const once = `$DATALIB_DAG_STEP/once-${Date.now()}`;
    const soured = `${config()}

[[groups]]
id = "soured"
type = "pdf"

[[steps]]
group = "soured"
function = "ingest"
command = "/bin/sh -c 'mkdir -p $DATALIB_DAG_STEP; if [ -e ${once} ]; then echo upstream went away >&2; exit 1; fi; touch ${once}'"
`;
    await writeConfig(page, soured);
    await expandGroup(page, "soured");
    expect(await lastSuccessOf(page, "soured/ingest")).toBeNull();

    await syncBtn(page, "soured/ingest").click();
    expect(await settle(page, "soured/ingest", null)).toBe("Succeeded");
    await expandGroup(page, "soured");
    const succeeded = await lastSyncedOf(page, "soured/ingest");
    expect(succeeded).not.toBeNull();
    expect(await lastSuccessOf(page, "soured/ingest")).toBe(succeeded);

    await untilTheSecondTurns();
    await syncBtn(page, "soured/ingest").click();
    expect(await settle(page, "soured/ingest", succeeded)).toBe("Failed");
    await expandGroup(page, "soured");
    const failed = await lastSyncedOf(page, "soured/ingest");
    expect(failed).not.toBe(succeeded);
    expect(await lastSuccessOf(page, "soured/ingest")).toBe(succeeded);
    await expect(
      groupRow(page, "soured").locator('[col-id="last_synced"] [title]'),
    ).toHaveAttribute("title", failed!);
    await expect(
      groupRow(page, "soured").locator('[col-id="last_success"] [title]'),
    ).toHaveAttribute("title", succeeded!);
  });

  test("Reset empties a source, and its documents leave the rows at once", async ({ page }) => {
    await writeConfigAndOpenGroups(page, config());
    const render = "pdfs/render_markdown";
    const documents = async () =>
      (await row(page, render).locator('[col-id="documents"]').innerText()).trim();

    const was = await stampsBefore(page, ["pdfs/ingest", render]);
    await syncBtn(page, "pdfs/ingest").click();
    await settleRow(page, "pdfs/ingest", was["pdfs/ingest"]);
    await settleRow(page, render, was[render]);
    await expect.poll(documents, { message: "the sync counted no documents" }).not.toMatch(/^0?$/);

    // The confirm says what a reset does, and the history is why it can
    // be a menu entry at all.
    let asked = "";
    page.on("dialog", (d) => {
      asked = d.message();
      void d.accept();
    });
    const rendered = await stampsBefore(page, [render]);
    await pickRowMenu(
      page,
      groupRow(page, "pdfs"),
      "Reset (preserve attachments)…",
      page.getByText("Reset pdfs."),
    );
    expect(asked).toContain("Every row goes, and the history keeps them");

    // Nothing more to click: the render catches up on the emptied store
    // by itself and takes its documents out.
    expect(await settleRow(page, render, rendered[render])).toMatch(/^(Succeeded|Up to date)$/);
    await expect.poll(documents, { message: "the documents stayed after a reset" }).toBe("0");
    // The download keeps no history of its own: its next Sync starts from
    // nothing.
    expect(await statusOf(page, "pdfs/ingest")).toBe("Never run");
    await settleRunner(page);
  });

  test("Reset on a render renders its documents again at once", async ({ page }) => {
    await writeConfigAndOpenGroups(page, config());
    const render = "pdfs/render_markdown";
    const documents = async () =>
      (await row(page, render).locator('[col-id="documents"]').innerText()).trim();

    const was = await stampsBefore(page, ["pdfs/ingest", render]);
    await syncBtn(page, "pdfs/ingest").click();
    await settleRow(page, "pdfs/ingest", was["pdfs/ingest"]);
    await settleRow(page, render, was[render]);
    await expect.poll(documents, { message: "the sync counted no documents" }).not.toMatch(/^0?$/);
    const counted = await documents();

    page.on("dialog", (d) => void d.accept());
    const rendered = await stampsBefore(page, [render]);
    await pickRowMenu(
      page,
      row(page, render),
      "Reset (preserve attachments)…",
      page.getByText("Reset Render markdown."),
    );
    // Rebuilt from what is downloaded, with nothing more to click; the
    // download itself is untouched.
    expect(await settleRow(page, render, rendered[render])).toBe("Succeeded");
    await expect.poll(documents).toBe(counted);
    expect(await statusOf(page, "pdfs/ingest")).toBe("Succeeded");
    await settleRunner(page);
  });

  test("a render whose code_version moved syncs on its own, and not while up to date", async ({
    page,
  }) => {
    await writeConfigAndOpenGroups(page, config());
    const render = "pdfs/render_markdown";
    const btn = syncBtn(page, render);

    const was = await stampsBefore(page, ["pdfs/ingest", render]);
    await syncBtn(page, "pdfs/ingest").click();
    await settleRow(page, "pdfs/ingest", was["pdfs/ingest"]);
    await settleRow(page, render, was[render]);
    await settleRunner(page);
    // Up to date, a Sync of it would do nothing, so it is disabled and
    // says so.
    await expect(btn).toBeDisabled();
    await expect(btn).toHaveAttribute("title", /^Up to date/);

    // A bumped code_version is what an upgrade that renders differently
    // looks like: the render is out of date, and its Sync reruns it alone.
    const bumped = config().replace(
      'inputs = ["pdfs/ingest"]\n',
      'inputs = ["pdfs/ingest"]\ncode_version = "bumped"\n',
    );
    expect(bumped).not.toBe(config());
    await writeConfigAndOpenGroups(page, bumped);
    await expect(btn).toBeEnabled();
    await expect(btn).toHaveAttribute("title", /^Out of date.*sync pdfs\/ingest/);
    const before = await stampsBefore(page, ["pdfs/ingest", render]);
    await btn.click();
    await settleRow(page, render, before[render]);
    await settleRunner(page);
    expect(await lastSyncedOf(page, "pdfs/ingest"), "the download did not run").toBe(
      before["pdfs/ingest"],
    );
    await expect(btn).toBeDisabled();
  });
});
