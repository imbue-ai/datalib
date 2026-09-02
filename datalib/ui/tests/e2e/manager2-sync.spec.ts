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

import { test, expect, type Page } from "@playwright/test";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.FW_E2E_DATALIB_STEP;
const FIXTURE_ROOT = process.env.FW_E2E_FIXTURE_ROOT;
const PDF_DIR = process.env.FW_E2E_PDF_FIXTURE_DIR;

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
}

const TERMINAL = /^(Succeeded|Up to date|Failed|Blocked|Interrupted)$/;

/// Wait for a row to settle on a terminal status, then return it.
/// Polls the DOM the way a person watches the screen.
///
/// `expect.poll` on the status string rather than a bespoke loop, so a
/// timeout reports the state it was stuck on ("expected 'Queued' to
/// match …") instead of just expiring.
async function settle(page: Page, id: string, timeout = 60_000): Promise<string> {
  await expect
    .poll(async () => (await statusOf(page, id)) ?? "(no status)", {
      timeout,
      intervals: [250],
      message: `${id} never reached a terminal status`,
    })
    .toMatch(TERMINAL);
  return (await statusOf(page, id)) ?? "";
}

let original = "";

test.beforeEach(async ({ page }) => {
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
    !STEP_BIN || !FIXTURE_ROOT || !PDF_DIR,
    "needs FW_E2E_DATALIB_STEP + FW_E2E_FIXTURE_ROOT + FW_E2E_PDF_FIXTURE_DIR from run_e2e.sh",
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

  const config = () => `data_root = "${FIXTURE_ROOT}"

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
input_path = "${FIXTURE_ROOT}/fsindex_scan"

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
input_path = "${FIXTURE_ROOT}/fsindex_scan"
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

    // Sample the row the way the grid paints it, from the click until
    // it settles. This is the real sequence — the unit suite replays a
    // synthetic one through the same state machine.
    const seen: string[] = [];
    const downstream: string[] = [];
    const record = async () => {
      const s = await statusOf(page, "pdfs/raw");
      if (s && seen[seen.length - 1] !== s) seen.push(s);
      const d = await statusOf(page, "pdfs/rendered_md");
      if (d && downstream[downstream.length - 1] !== d) downstream.push(d);
      return s;
    };

    await syncBtn(page, "pdfs/raw").click();
    // The first sample is deliberately taken with no await in between:
    // the push from the enqueue handler is what has to have landed.
    await record();

    // Syncing a source claims everything downstream of it, so the
    // render step is queued from the same first frame — before the
    // runner exists, let alone reaches it. This is the assertion a
    // download-only provider could not support, and the reason this
    // spec is built on `pdf`.
    expect(downstream[0], `downstream sequence was ${JSON.stringify(downstream)}`).toBe(
      "Queued",
    );
    // ...while the unrelated source is not claimed at all.
    expect(await statusOf(page, "docs/raw")).not.toBe("Queued");

    const deadline = Date.now() + 60_000;
    // `TERMINAL`, not a set spelled out again here. The local copy this
    // replaces listed three of the five terminal statuses, so a run
    // ending `Blocked` or `Interrupted` was never recognized as over:
    // the loop spun to the deadline and reported "never settled" about a
    // row that had settled a minute earlier, naming neither the status
    // nor the reason.
    for (;;) {
      const s = await record();
      if (s && TERMINAL.test(s)) break;
      expect(Date.now(), `never settled; saw ${JSON.stringify(seen)}`).toBeLessThan(
        deadline,
      );
      await page.waitForTimeout(150);
    }

    // What the sequence must contain. "Queued" is the frame that used
    // to be missing entirely — the click produced no visible change
    // until the whole run was over.
    expect(seen[0], `sequence was ${JSON.stringify(seen)}`).toBe("Queued");
    expect(seen[seen.length - 1]).toBe("Succeeded");

    // ...and it must be monotonic. A status going backwards reads as
    // "about to run again", which is worse than a stale one. Running is
    // optional here — a scan of a small tree can finish inside one
    // sample — but if it appears it must be in the right place.
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
    expect(await settle(page, "pdfs/rendered_md")).toMatch(/^(Succeeded|Up to date)$/);
    await record(); // capture the settled state in the sequence
    expect(
      downstream[0],
      `downstream never started Queued: ${JSON.stringify(downstream)}`,
    ).toBe("Queued");
    expect(
      downstream[downstream.length - 1],
      `downstream never finished: ${JSON.stringify(downstream)}`,
    ).toMatch(/^(Succeeded|Up to date)$/);
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
