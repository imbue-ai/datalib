// The whole first hour, in one test: an empty folder becomes a data
// library with a source in it, that source is synced, and what it
// produced is in the grid — then a file lands in the watched folder and
// the second sync picks it up.
//
// Everything here is driven from the screen. There is no `writeConfig`
// helper and no API call that changes state: the config this test runs
// on is written by the button that initializes the root, by the wizard,
// and by the row's delete action, exactly as a person's would be. The
// point is that the *seams between the screens* work — the first-run
// gate hands over to the Pipeline table, the wizard's TOML is a config
// the runner accepts, the runner's record reaches the row that started
// it, and the applet serves what the pipeline just wrote.
//
// Its own backend, on its own empty root (FW_E2E_ONBOARDING_URL, see
// playwright.config.ts): the onboarding state is one-shot, and
// `first-run.spec.ts` already consumes the other empty root. That root
// is also the one server here whose PATH carries the dash-named
// binaries, because a scaffold-written config names them bare — which
// is what an installed user's config does.
//
// **Why the qmd index step is deleted first.** The scaffold declares
// two fan-ins, and `qmd_index` shells out to a node runtime and loads
// ~1.6 GB of embedding models. It is real work this test has no opinion
// about and cannot afford; `search-qmd-routing.spec.ts` is where qmd is
// covered. Deleting it is done the way a user would — the row's own
// delete action — so the removal is itself a small assertion that the
// action works.

import { test, expect, type Page } from "@playwright/test";
import { copyFileSync } from "node:fs";
import { expectGridPainted } from "./grid-helpers";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const BASE = process.env.FW_E2E_ONBOARDING_URL;
/// The folder the source scans. Seeded by playwright.config.ts with the
/// two Captain's Log PDFs; the third arrives mid-test.
const SCAN_DIR = process.env.FW_E2E_PDF_SCAN_DIR;
/// The held-back document, copied in at step 12.
const LATECOMER = process.env.FW_E2E_PDF_LATECOMER;

const row = (page: Page, id: string) => page.locator(`.ag-row[row-id="${id}"]`);

/// A row's status, which the column paints as an icon — so the state is
/// the icon's accessible name. Null while the cell is mid-repaint.
async function statusOf(page: Page, id: string): Promise<string | null> {
  const el = row(page, id).locator('[col-id="status"] [role="img"]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("aria-label");
}

const TERMINAL = /^(Succeeded|Up to date|Failed|Blocked|Interrupted)$/;

/// The exact instant a row last ran, off the Last-synced cell's
/// `title`. Null for a row that has never run, which renders "—" with
/// no title to read.
async function stampOf(page: Page, id: string): Promise<string | null> {
  const el = row(page, id).locator('[col-id="lastSynced"] [title]');
  if ((await el.count()) === 0) return null;
  return await el.first().getAttribute("title");
}

/// Wait for a row to finish a run that started *after* `before`, and
/// return the status it settled on.
///
/// Keyed on the row's timestamp changing, not on its status reaching a
/// terminal word. "Terminal" cannot tell a finished run from the
/// previous one: a second sync of an already-succeeded row leaves
/// "Succeeded" on screen until the job claims it a moment later, so a
/// status-only wait passes instantly against the *old* run and then
/// reports whatever the row happens to say next — which is "Queued",
/// the very state it was supposed to wait out. That is a race the
/// first sync of a never-run row cannot show (its status starts
/// "Never run" and its stamp starts null), which is why it survived
/// until a second sync was added.
///
/// `expect.poll` rather than a bespoke loop so a timeout reports the
/// state it was stuck on instead of just expiring.
async function settle(
  page: Page,
  id: string,
  before: string | null,
  timeout = 60_000,
): Promise<string> {
  let last = "(no status)";
  await expect
    .poll(
      async () => {
        last = (await statusOf(page, id)) ?? "(no status)";
        const stamp = await stampOf(page, id);
        return TERMINAL.test(last) && stamp !== before ? "finished" : `${last} @ ${stamp}`;
      },
      {
        timeout,
        intervals: [200],
        message: `${id} never finished a run newer than ${before ?? "(never run)"}`,
      },
    )
    .toBe("finished");
  return last;
}

/// The three rows a sync of `pdfs/raw` drives, and what each is
/// claiming before it starts — captured together so the settle below
/// can tell this run's result from the last one's.
const SYNCED_ROWS = ["pdfs/raw", "pdfs/rendered_md", "unified_index/grid"] as const;
async function stampsBefore(page: Page): Promise<Record<string, string | null>> {
  const out: Record<string, string | null> = {};
  for (const id of SYNCED_ROWS) out[id] = await stampOf(page, id);
  return out;
}

/// "Bytes on disk" as a number, read back off the label drawn over the
/// bar — the number a person actually sees. `null` for a row with
/// nothing on disk, which the column renders as an em dash rather than
/// as a zero-length bar.
///
/// The label is rounded to three significant figures, so this is only
/// good for comparisons coarser than that. The growth it is asked about
/// here is ~23 kB against ~142 kB, which is far outside that.
async function bytesOf(page: Page, id: string): Promise<number | null> {
  const label = row(page, id).locator('[col-id="bytes"] .m2-bar-label');
  if ((await label.count()) === 0) return null;
  const text = ((await label.first().textContent()) ?? "").trim();
  const m = /^([\d.]+)\s*(B|kB|MB|GB|TB)$/.exec(text);
  expect(m, `unparsable size in the Bytes column: ${JSON.stringify(text)}`).not.toBeNull();
  const scale = { B: 1, kB: 1e3, MB: 1e6, GB: 1e9, TB: 1e12 }[m![2]]!;
  return Number(m![1]) * scale;
}

/// Every row the Explore grid is holding, read through the grid api the
/// GridCard exposes.
///
/// Through the api rather than off the DOM because AG Grid virtualizes
/// *columns* as well as rows: whether "Author" has a DOM node at all
/// depends on the viewport width, so a `getByText` assertion on it
/// would pass or fail on window size. The painted-ness of the grid is
/// asserted separately, by `expectGridPainted`.
async function gridRows(
  page: Page,
): Promise<{ sender: string; conversation_name: string; source: string }[]> {
  return await page.evaluate(() => {
    type Node = { data?: { sender: string; conversation_name: string; source: string } };
    const api = (
      window as unknown as {
        __fwGridApi?: { forEachNode: (cb: (n: Node) => void) => void };
      }
    ).__fwGridApi!;
    const out: { sender: string; conversation_name: string; source: string }[] = [];
    api.forEachNode((n) => {
      if (n.data) out.push(n.data);
    });
    return out;
  });
}

/// Open Explore and wait for it to have painted rows from the applet.
async function openExplore(page: Page) {
  await page.goto(`${BASE}/`);
  await expect(page.locator('.ag-center-cols-container [role="row"]').first()).toBeVisible({
    timeout: 20_000,
  });
  await expectGridPainted(page.locator(".ag-root-wrapper").first(), "Explore grid");
}

test.describe("onboarding: empty folder → indexed PDFs", () => {
  // One real `datalib-dag` run per sync, on a cold data root. The first
  // one creates the doltlite stores, which is most of its cost.
  test.setTimeout(120_000);

  test.skip(
    !BASE || !SCAN_DIR || !LATECOMER,
    "needs FW_E2E_ONBOARDING_URL + FW_E2E_PDF_SCAN_DIR + FW_E2E_PDF_LATECOMER from playwright.config.ts",
  );

  test("a new library indexes a PDF folder, and picks up a file added later", async ({
    page,
    request,
  }) => {
    // Both destructive row actions go through `window.confirm`, which
    // Playwright otherwise auto-dismisses — a dismissed confirm reads
    // as "no" and the click would silently do nothing.
    page.on("dialog", (d) => void d.accept());

    // ── 1-2. an empty folder, and the gate that fills it ─────────────
    const before = await request.get(`${BASE}/api/config`);
    expect(
      (await before.json()).exists,
      "this root must start with no config — it is the state under test",
    ).toBe(false);

    await page.goto(`${BASE}/`);
    await expect(page.getByRole("heading", { name: "Set up a data library" })).toBeVisible();
    await page.getByRole("button", { name: "Initialize empty data library" }).click();

    // ── 3. landing in Manager2 ───────────────────────────────────────
    await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
    expect(new URL(page.url()).pathname).toBe("/sources2");
    // The scaffold's three entries are the table's whole content.
    for (const id of ["unified_index/grid", "unified_index/qmd", "unified_index"]) {
      await expect(row(page, id)).toHaveCount(1);
    }

    // The qmd index step, removed before anything can queue it. See the
    // header: it is real work this test cannot afford, and the delete
    // action is the honest way to not run it.
    await row(page, "unified_index/qmd")
      .getByRole("button", { name: "Remove from config" })
      .click();
    await expect(row(page, "unified_index/qmd")).toHaveCount(0);

    // ── 4-6. the wizard ──────────────────────────────────────────────
    await page.getByRole("button", { name: "+ Add Data Source" }).click();
    const wizard = page.getByRole("dialog");
    await wizard.getByRole("searchbox").fill("pdf");
    await wizard.getByRole("button", { name: /PDFs/ }).click();

    await wizard.locator("input.wiz-path").fill(SCAN_DIR!);

    // Rendering to markdown is what makes the documents searchable, so
    // it is offered here rather than as a second dialog — `pdf`'s
    // render step has no settings of its own. Ticked by default; this
    // test wants it, and says so rather than assuming.
    const alsoRender = wizard.getByRole("checkbox");
    await expect(alsoRender).toBeChecked();

    // What the two steps will be, named from the catalog's default id,
    // shown before anything is written.
    await wizard.getByText("Review the TOML this writes").click();
    const toml = wizard.locator("pre");
    await expect(toml).toContainText('id = "pdfs/raw"');
    await expect(toml).toContainText(`input_path = "${SCAN_DIR}"`);
    await expect(toml).toContainText('id = "pdfs/rendered_md"');
    await expect(toml).toContainText('inputs = ["pdfs/raw"]');

    await wizard.getByRole("button", { name: "Add source" }).click();
    await expect(wizard).toHaveCount(0);

    // Two rows, and neither has ever run: no status history, nothing on
    // disk. This is the state the sync below has to move.
    await expect(row(page, "pdfs/raw")).toHaveCount(1);
    await expect(row(page, "pdfs/rendered_md")).toHaveCount(1);
    expect(await statusOf(page, "pdfs/raw")).toBe("Never run");
    expect(await bytesOf(page, "pdfs/raw")).toBeNull();
    await expect(row(page, "pdfs/raw").locator('[col-id="lastSynced"]')).toHaveText("—");

    // The render step was wired into the surviving fan-in, which is
    // what gets these documents indexed rather than merely converted.
    const wired = await (await request.get(`${BASE}/api/config`)).json();
    expect(wired.parsed_ok, wired.error ?? "config must load").toBe(true);
    expect(wired.text).toContain('inputs = ["pdfs/rendered_md"]');

    // ── 7-8. run it ──────────────────────────────────────────────────
    const firstRun = await stampsBefore(page);
    await row(page, "pdfs/raw").getByRole("button", { name: "Sync now" }).click();

    // Download, render and index all run — syncing a source claims
    // everything downstream of it, and the index step is the reason the
    // grid below has anything in it.
    expect(await settle(page, "pdfs/raw", firstRun["pdfs/raw"])).toBe("Succeeded");
    expect(
      await settle(page, "pdfs/rendered_md", firstRun["pdfs/rendered_md"]),
    ).toMatch(/^(Succeeded|Up to date)$/);
    expect(
      await settle(page, "unified_index/grid", firstRun["unified_index/grid"]),
    ).toMatch(/^(Succeeded|Up to date)$/);

    // ── 9. the two columns that report it ────────────────────────────
    const cell = row(page, "pdfs/raw").locator('[col-id="lastSynced"]');
    await expect(cell).toHaveText(/(just now|\d+ seconds? ago)/);
    const stamp = await stampOf(page, "pdfs/raw");
    expect(stamp, "the relative text must not be the only record").toBeTruthy();
    expect(
      Math.abs(Date.now() - Date.parse(stamp!)),
      `Last synced claims ${stamp}, which is not a moment ago`,
    ).toBeLessThan(5 * 60_000);

    const rawBytes = await bytesOf(page, "pdfs/raw");
    const renderedBytes = await bytesOf(page, "pdfs/rendered_md");
    expect(rawBytes, "the raw store should be on disk now").toBeGreaterThan(0);
    expect(renderedBytes, "so should the markdown").toBeGreaterThan(0);

    // ── 10-11. the documents, in the grid ────────────────────────────
    await openExplore(page);
    const first = await gridRows(page);
    expect(first.length, "the PDFs should be indexed").toBeGreaterThan(0);
    expect(
      first.every((r) => r.source === "PDF"),
      `every row should come from the one source configured: ${JSON.stringify(first)}`,
    ).toBe(true);
    expect(first.map((r) => r.conversation_name)).toContain("Captain's Log");
    // …and the document held back from the folder is not there, which
    // is what makes its arrival below mean something.
    expect(first.map((r) => r.sender)).not.toContain("Geordi La Forge");

    // ── 12-14. a file appears in the folder ──────────────────────────
    copyFileSync(LATECOMER!, `${SCAN_DIR}/warp_core_manual.pdf`);

    await page.goto(`${BASE}/sources2`);
    await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
    // Re-read rather than reuse: this is a fresh page, and the numbers
    // it shows are the ones the assertion below is about.
    const beforeSecond = await bytesOf(page, "pdfs/raw");
    expect(beforeSecond).toBe(rawBytes);

    const secondRun = await stampsBefore(page);
    await row(page, "pdfs/raw").getByRole("button", { name: "Sync now" }).click();
    expect(await settle(page, "pdfs/raw", secondRun["pdfs/raw"])).toBe("Succeeded");
    expect(
      await settle(page, "pdfs/rendered_md", secondRun["pdfs/rendered_md"]),
    ).toMatch(/^(Succeeded|Up to date)$/);
    expect(
      await settle(page, "unified_index/grid", secondRun["unified_index/grid"]),
    ).toMatch(/^(Succeeded|Up to date)$/);

    // A document more on disk.
    //
    // **The rendered_md row is the one that can fail here.** It is a
    // plain file tree, so a run that converts nothing adds nothing to
    // it; only a document that really was new makes it grow. The raw
    // row grows on *every* run whether or not anything changed, so a
    // strict increase there would pass for any re-sync. It is checked
    // with a floor a no-op cannot clear, and the render row strictly.
    //
    // That per-run growth is *not* a commit, though it reads like one:
    // measured over ten no-op runs, `dolt_log` stays put for most of
    // them and the store still grows anyway. The bulk of it was the
    // schema self-heal building its probe table in the store itself —
    // create+drop nets to nothing in the working tree, so nothing
    // commits and nothing reads as dirty, but the chunks stay — one
    // probe per table per open, which `doltlite_raw::declared_columns`
    // now does in memory instead. What is left is doltlite's own
    // storage churn per write session.
    //
    // **The floor is calibrated to that, so it moved when the leak
    // did.** Measured by running `datalib-step download pdf` over a
    // copy of this scan directory: no-op runs add 2165 B once and then
    // 507-676 B, and the run that picks up the late PDF adds 8279 B.
    // 4 kB is above the worst no-op and half the real gain. The old
    // 10 kB was above *both* once the probe stopped padding every run
    // — which is how a real ingest came to fail an assertion about
    // ingests, rather than by anything changing about the ingest.
    //
    // Both numbers are small and the margin is ~2x, so re-measure
    // rather than nudge this constant if it ever goes red: the two
    // quantities it sits between are the whole point of the check.
    //
    // Polled, because the storage figures are refetched when the run
    // goes terminal rather than painted from the event that says so.
    await expect
      .poll(async () => (await bytesOf(page, "pdfs/rendered_md")) ?? 0, {
        timeout: 10_000,
        intervals: [250],
        message: `the rendered markdown never grew past ${renderedBytes}`,
      })
      .toBeGreaterThan(renderedBytes!);
    expect(
      (await bytesOf(page, "pdfs/raw"))!,
      "the raw store should have gained a document, not just per-run churn",
    ).toBeGreaterThan(rawBytes! + 4_000);

    // ── 15. and it is searchable ─────────────────────────────────────
    await openExplore(page);
    const second = await gridRows(page);
    expect(second.length, "the new document should have added rows").toBeGreaterThan(
      first.length,
    );
    expect(
      second.map((r) => r.conversation_name),
      "the document added to the folder should be in the grid",
    ).toContain("warp_core_manual.pdf");
    expect(second.map((r) => r.sender)).toContain("Geordi La Forge");
  });
});
