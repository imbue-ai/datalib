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

/// "Last synced" as the grid renders it — the string a person reads.
async function lastSyncedOf(page: Page, id: string): Promise<string> {
  return (
    (await row(page, id).locator('[col-id="lastSynced"]').first().textContent()) ?? ""
  ).trim();
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
    expect(docsSynced).not.toBe("—");

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
    const terminal = new Set(["Succeeded", "Up to date", "Failed"]);
    for (;;) {
      const s = await record();
      if (s && terminal.has(s)) break;
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
    const rank: Record<string, number> = { Queued: 0, Running: 1, Succeeded: 2 };
    for (let i = 1; i < seen.length; i++) {
      expect(
        rank[seen[i]],
        `went backwards: ${JSON.stringify(seen)}`,
      ).toBeGreaterThanOrEqual(rank[seen[i - 1]]);
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
