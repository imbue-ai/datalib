// The whole first hour, in one test: an empty folder becomes a data
// library with a source in it, that source is synced, and what it
// produced is in the grid — then a file lands in the watched folder and
// the second sync picks it up.

import { test, expect, type Page } from "@playwright/test";
import { copyFileSync } from "node:fs";
import {
  expandGroup,
  expectGridPainted,
  groupRow,
  pipelineRow as row,
  searchAndSettle,
  settle,
  settleRows,
  stampOf,
  stampsBefore,
  statusOf,
} from "./grid-helpers";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const BASE = process.env.FW_E2E_ONBOARDING_URL;
/// The folder the source scans. Seeded by playwright.config.ts with the
/// two Captain's Log PDFs; the third arrives mid-test.
const SCAN_DIR = process.env.FW_E2E_PDF_SCAN_DIR;
/// The held-back document, copied in at step 12.
const LATECOMER = process.env.FW_E2E_PDF_LATECOMER;
/// The folder holding the generated `signal-backup-*` snapshot, which
/// is what a Signal source's "Backup folder" field wants — the
/// downloader scans it for the newest snapshot rather than being handed
/// one. Built by playwright.config.ts from the checked-in TNG spec.
const SIGNAL_BACKUP_DIR = process.env.FW_E2E_SIGNAL_BACKUP_DIR;

/// The three rows a sync of `pdfs/raw` drives: the source, its render
/// sibling, and the fan-in that makes the documents searchable.
const SYNCED_ROWS = ["pdfs/raw", "pdfs/rendered_md", "unified_index/grid"];

/// "Bytes on disk" as a number, read back off the label drawn over the
/// sparkline — the number a person actually sees. `null` for a row with
/// nothing on disk, which the column renders as an em dash rather than
/// as a flat line at zero.
async function bytesOf(page: Page, id: string): Promise<number | null> {
  const label = row(page, id).locator('[col-id="bytes"] .m2-plot-label');
  if ((await label.count()) === 0) return null;
  const text = ((await label.first().textContent()) ?? "").trim();
  const m = /^([\d.]+)\s*(B|kB|MB|GB|TB)$/.exec(text);
  expect(m, `unparsable size in the Bytes column: ${JSON.stringify(text)}`).not.toBeNull();
  const scale = { B: 1, kB: 1e3, MB: 1e6, GB: 1e9, TB: 1e12 }[m![2]]!;
  return Number(m![1]) * scale;
}

/// Every row the Explore grid is holding, read through the grid api the
/// GridCard exposes.
async function gridRows(
  page: Page,
): Promise<
  { sender: string; conversation_name: string; source: string; source_name: string }[]
> {
  return await page.evaluate(() => {
    type Node = {
      data?: {
        sender: string;
        conversation_name: string;
        source: string;
        source_name: string;
      };
    };
    const api = (
      window as unknown as {
        __fwGridApi?: { forEachNode: (cb: (n: Node) => void) => void };
      }
    ).__fwGridApi!;
    const out: {
      sender: string;
      conversation_name: string;
      source: string;
      source_name: string;
    }[] = [];
    api.forEachNode((n) => {
      if (n.data) out.push(n.data);
    });
    return out;
  });
}

/// Open Explore and wait for it to have painted rows from the applet.
async function openExplore(page: Page) {
  await page.goto(`${BASE}/`);
  await expect(page.locator('.ag-grid-scrolling-rows [role="row"]').first()).toBeVisible({
    timeout: 20_000,
  });
  await expectGridPainted(page.locator(".ag-root-wrapper").first(), "Explore grid");
}

// Record this file, always — video and trace, passing or failing.
//
// `"on"` rather than `"retain-on-failure"` on purpose: a recording that
// only exists after a failure cannot answer "what did this look like
// before?", which is the question a regression usually raises. The rest
// of the suite keeps the cheaper `retain-on-failure` from the top-level
// `use` block; this is the one file that pays for always-on.
test.use({ video: "on", trace: "on" });

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
    // The scaffold's one group is the table's whole content, and its
    // three entries are under it.
    await expect(groupRow(page, "unified_index")).toContainText("Unified Index");
    await expect(page.locator(".ag-row")).toHaveCount(1);
    await expandGroup(page, "unified_index");
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
    await expect(toml).toContainText('id = "pdfs"');
    await expect(toml).toContainText('type = "pdf"');
    await expect(toml).toContainText('function = "raw"');
    await expect(toml).toContainText(`input_path = "${SCAN_DIR}"`);
    await expect(toml).toContainText('function = "rendered_md"');
    await expect(toml).toContainText('inputs = ["pdfs/raw"]');

    await wizard.getByRole("button", { name: "Add source" }).click();
    await expect(wizard).toHaveCount(0);

    // One group row, with two steps under it, and none of them has ever
    // run: no status history, nothing on disk. This is the state the
    // sync below has to move. The group reads off its steps, so it
    // says the same.
    await expect(groupRow(page, "pdfs")).toContainText("PDFs");
    expect(await statusOf(page, "group:pdfs")).toBe("Never run");
    expect(await bytesOf(page, "group:pdfs")).toBeNull();
    await expandGroup(page, "pdfs");
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
    const firstRun = await stampsBefore(page, SYNCED_ROWS);
    await row(page, "pdfs/raw").getByRole("button", { name: "Sync now" }).click();

    // Download, render and index all run — syncing a source claims
    // everything downstream of it, and the index step is the reason the
    // grid below has anything in it.
    const firstDone = await settleRows(page, SYNCED_ROWS, firstRun);
    expect(firstDone["pdfs/raw"]).toBe("Succeeded");
    expect(firstDone["pdfs/rendered_md"]).toMatch(/^(Succeeded|Up to date)$/);
    expect(firstDone["unified_index/grid"]).toMatch(/^(Succeeded|Up to date)$/);

    // ── 9. the two columns that report it ────────────────────────────
    const cell = row(page, "pdfs/raw").locator('[col-id="lastSynced"]');
    await expect(cell).toHaveText("seconds ago");
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
    // `source_name`, not `source`: the question is whether a row leaked
    // in from another *configured source*, and this library has exactly
    // one. `source` is the provider label, and the storage rows every
    // source now emits carry "Storage" there while still belonging to
    // this one — see docs/dev/grid_rows.md.
    expect(
      first.every((r) => r.source_name === "pdfs"),
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

    const secondRun = await stampsBefore(page, SYNCED_ROWS);
    await row(page, "pdfs/raw").getByRole("button", { name: "Sync now" }).click();
    const secondDone = await settleRows(page, SYNCED_ROWS, secondRun);
    expect(secondDone["pdfs/raw"]).toBe("Succeeded");
    expect(secondDone["pdfs/rendered_md"]).toMatch(/^(Succeeded|Up to date)$/);
    expect(secondDone["unified_index/grid"]).toMatch(/^(Succeeded|Up to date)$/);

    // A document more on disk.
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

  test("a second source joins the first, and Sync everything brings both up to date", async ({
    page,
  }) => {
    // Runs against the root the test above built, in declaration order
    // under `workers: 1`. That is the point rather than a shortcut: a
    // library with *one* source cannot show any of what follows. Only a
    // second source makes "this row synced and that one never did" a
    // distinguishable state, and only then does a button that runs
    // everything mean something a per-row Sync does not.
    test.skip(
      !SIGNAL_BACKUP_DIR,
      "needs FW_E2E_SIGNAL_BACKUP_DIR — signal_make_fixture from run_e2e.sh",
    );
    page.on("dialog", (d) => void d.accept());

    await page.goto(`${BASE}/sources2`);
    await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
    // A fresh browser context: the groups are folded again.
    await expandGroup(page, "pdfs");

    // ── 1. add Signal through the wizard ─────────────────────────────
    const wizard = page.getByRole("dialog");
    await page.getByRole("button", { name: "+ Add Data Source" }).click();
    await wizard.getByRole("searchbox").fill("signal");
    await wizard
      .locator(".wiz-tile", { hasText: "Decrypt and mirror an Android Signal backup" })
      .click();
    await wizard.locator("input.wiz-path").fill(SIGNAL_BACKUP_DIR!);

    // The render step has a `period` option, so the offer is a confirm
    // and then a second dialog — not the checkbox `pdf` got. The
    // `page.on("dialog")` above accepts it.
    await expect(wizard.locator("label.wiz-check")).toHaveCount(0);
    await wizard.getByRole("button", { name: "Add source" }).click();
    // The second dialog, for the render step. Its defaults are what we
    // want; taking them is still a click a person makes.
    await expect(wizard.getByRole("button", { name: "Add render step" })).toBeEnabled();
    await wizard.getByRole("button", { name: "Add render step" }).click();
    await expect(wizard).toHaveCount(0);

    // ── 2. two sources in the table ──────────────────────────────────
    await expandGroup(page, "signal");
    for (const id of ["pdfs/raw", "pdfs/rendered_md", "signal/raw", "signal/rendered_md"]) {
      await expect(row(page, id), `${id} should be a row`).toHaveCount(1);
    }

    // ── 3. one has history, the other has none ───────────────────────
    //
    // The distinction the whole test rests on, so it is asserted on
    // both columns: a never-run row has no state to report *and* no
    // instant to report it at.
    expect(await statusOf(page, "pdfs/raw")).toBe("Succeeded");
    expect(await statusOf(page, "signal/raw")).toBe("Never run");
    await expect(row(page, "signal/raw").locator('[col-id="lastSynced"]')).toHaveText("—");
    expect(
      await stampOf(page, "signal/raw"),
      "a row that never ran has no instant to reveal",
    ).toBeNull();
    expect(
      await bytesOf(page, "signal/raw"),
      "a source that never ran has written nothing",
    ).toBeNull();

    // ── 4. re-running one source leaves the other alone ──────────────
    const pdfBefore = await stampOf(page, "pdfs/raw");
    await row(page, "pdfs/raw").getByRole("button", { name: "Sync now" }).click();
    expect(await settle(page, "pdfs/raw", pdfBefore)).toBe("Succeeded");
    expect(
      await statusOf(page, "signal/raw"),
      "a sync of pdfs must not give signal a history it never earned",
    ).toBe("Never run");
    expect(await stampOf(page, "signal/raw")).toBeNull();

    // ── 5. Sync everything reaches both ──────────────────────────────
    const ALL = ["pdfs/raw", "pdfs/rendered_md", "signal/raw", "signal/rendered_md"];
    const was = await stampsBefore(page, ALL);

    await page.getByRole("button", { name: "Sync everything" }).click();
    const done = await settleRows(page, ALL, was);
    for (const id of ALL) {
      expect(done[id], `${id} after Sync everything`).toMatch(/^(Succeeded|Up to date)$/);
    }

    // Everything now has a history and something on disk. `signal/raw`
    // is the row that proves it: it had neither a moment ago, and no
    // per-row button was pressed for it.
    for (const id of ALL) {
      await expect(
        row(page, id).locator('[col-id="lastSynced"]'),
        `${id} should report when it last ran`,
      ).not.toHaveText("—");
      expect(await bytesOf(page, id), `${id} should have bytes on disk`).toBeGreaterThan(0);
    }

    // ── 6. and the Signal messages are searchable ────────────────────
    await openExplore(page);
    await searchAndSettle(page, "source:Signal type:all");
    const signalRows = await gridRows(page);
    expect(signalRows.length, "the Signal messages should be indexed").toBeGreaterThan(0);
    expect(
      signalRows.every((r) => r.source === "Signal"),
      `every row should be from Signal: ${JSON.stringify(signalRows.slice(0, 5))}`,
    ).toBe(true);
  });
});
