// Driving a real sync from the Pipeline grid, and watching the whole
// sequence it produces.
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
// `pdf` is the local-only provider that has *both* halves, which is why
// it carries this spec: an `ingest -> render_markdown` edge is what makes
// "everything downstream is queued too" a real assertion about the DAG
// rather than a contrived one. `fsindex` (download-only) is the
// unrelated second source — the one whose history must not move.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  expandGroup,
  pipelineRow as row,
  recordStatuses,
  settle,
  settleRow,
  settleRunner,
  stampOf as lastSyncedOf,
  stampsBefore,
  statusLog,
  statusWord,
  statusOf,
  TERMINAL,
} from "./grid-helpers";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const STEP_BIN = process.env.FW_E2E_DATALIB_STEP;
const PDF_DIR = process.env.FW_E2E_PDF_FIXTURE_DIR;

/// This spec's own data root, asked of the backend rather than read
/// from the environment.
let dataRoot = "";
async function resolveDataRoot(request: APIRequestContext): Promise<string> {
  const { path } = (await (await request.get("/api/config")).json()) as {
    path: string;
  };
  return path.slice(0, path.lastIndexOf("/"));
}

const syncBtn = (page: Page, id: string) =>
  row(page, id).getByRole("button", { name: "Sync now" });

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
  // fetches config, jobs and the DAG record in one `Promise.all`, so
  // that in-between state cannot be observed.
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

  test("syncing one source leaves another source's history untouched", async ({
    page,
  }) => {
    await writeConfigAndOpenGroups(page, config());

    // Give docs a real history to protect.
    const docsWas = await lastSyncedOf(page, "docs/ingest");
    await syncBtn(page, "docs/ingest").click();
    expect(await settle(page, "docs/ingest", docsWas)).toBe("Succeeded");
    const docsStatus = await statusOf(page, "docs/ingest");
    const docsSynced = await lastSyncedOf(page, "docs/ingest");
    expect(docsSynced, "a synced row should carry an exact stamp").toBeTruthy();

    // Now sync the *other* source. The runner still walks docs/raw, to
    // publish its output version, and reports it `not_selected` — the
    // fact that used to be written over its record.
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

  test("the row shows the sync happening, and never goes backwards", async ({
    page,
  }) => {
    await writeConfigAndOpenGroups(page, config());

    // Watch the row the way the grid paints it, from before the click
    // until it settles. This is the real sequence — the unit suite
    // replays a synthetic one through the same state machine.
    await recordStatuses(page, ["pdfs/ingest", "pdfs/render_markdown"]);
    // Whatever the rows say before the click. The recorder seeds itself
    // with the current value, so this is 1 for a row with a status and
    // 0 for one still painting; everything past it is what the click
    // caused.
    const beforeUp = (await statusLog(page, "pdfs/ingest")).length;
    const beforeDown = (await statusLog(page, "pdfs/render_markdown")).length;

    const was = await stampsBefore(page, ["pdfs/ingest", "pdfs/render_markdown"]);
    await syncBtn(page, "pdfs/ingest").click();
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
    const downstream = (await statusLog(page, "pdfs/render_markdown")).slice(beforeDown);
    expect(
      statusWord(downstream[0]),
      `downstream sequence was ${JSON.stringify(downstream)}`,
    ).toBe("Queued");
    // ...while the unrelated source is not claimed at all.
    expect(await statusOf(page, "docs/ingest")).not.toBe("Queued");

    // `settleRow`, not `settle`: the log lives in the page, and
    // `settle` remounts, which would throw it away. The before-stamp is
    // still passed — a terminal status on its own is answerable by the
    // *previous* run's frame, which is what #237 fixed.
    await settleRow(page, "pdfs/ingest", was["pdfs/ingest"]);
    const seen = (await statusLog(page, "pdfs/ingest")).slice(beforeUp);

    // What the sequence must contain. "Queued" is the frame that used
    // to be missing entirely — the click produced no visible change
    // until the whole run was over.
    expect(statusWord(seen[0]), `sequence was ${JSON.stringify(seen)}`).toBe("Queued");
    expect(statusWord(seen[seen.length - 1])).toBe("Succeeded");

    // "Running" stays optional, and recording the transitions is what
    // settled *why*.
    //
    // The sampler this replaces guessed: "a scan of a small tree can
    // finish inside one sample". It could not tell a status that never
    // appeared from one it blinked past, so it had to allow both. The
    // recorder can, and the answer is the first: on this fixture the
    // sequence is `["Queued","Succeeded"]` — the row never paints
    // Running at all.

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
    const rankOf = (frame: string) => {
      const s = statusWord(frame);
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
    // reach a terminal state before its input does. `pdfs/ingest` is
    // already terminal here, so waiting on the render is bounded.
    expect(await settleRow(page, "pdfs/render_markdown", was["pdfs/render_markdown"])).toMatch(
      /^(Succeeded|Up to date)$/,
    );
    const downstreamFinal = (await statusLog(page, "pdfs/render_markdown")).slice(beforeDown);
    expect(
      statusWord(downstreamFinal[0]),
      `downstream never started Queued: ${JSON.stringify(downstreamFinal)}`,
    ).toBe("Queued");
    expect(
      statusWord(downstreamFinal[downstreamFinal.length - 1]),
      `downstream never finished: ${JSON.stringify(downstreamFinal)}`,
    ).toMatch(/^(Succeeded|Up to date)$/);

    // The run itself has to be over before the next test writes a
    // config into this root — the half of `settleRows` that `settleRow`
    // leaves out. Cheap here: every row is already terminal.
    await settleRunner(page);
  });

  test("Last synced holds still under a minute, and hover reveals the exact stamp", async ({
    page,
  }) => {
    // What only a browser can answer about this column. The arithmetic
    // — every unit boundary, a stamp in another UTC offset, one in the
    // future — is in src/config/timeFormat.test.ts, because provoking
    // "6 days ago" from a live backend would mean forging the runner's
    // state file. What that unit test cannot show is any of the below:
    // that the column is wired to the relative form at all, that the
    // absolute stamp survives as the hover, and that the cell does not
    // tick while a person is looking at it.
    await writeConfigAndOpenGroups(page, config());
    const countUpWas = await lastSyncedOf(page, "pdfs/ingest");
    await syncBtn(page, "pdfs/ingest").click();
    expect(await settle(page, "pdfs/ingest", countUpWas)).toBe("Succeeded");

    const cell = row(page, "pdfs/ingest").locator('[col-id="lastSynced"]');
    await expect(cell).toHaveText("seconds ago");

    // The exact instant is still reachable, on the hover.
    const stamp = await lastSyncedOf(page, "pdfs/ingest");
    expect(stamp, "the relative text must not be the only record").toBeTruthy();
    expect(stamp).toMatch(/\d{2}:\d{2}:\d{2}/);

    // ...and it stays put. The repaint loop is live and sampling
    // `Date.now()` every second throughout this window, so against the
    // per-second countup this column used to do, these samples would
    // have read 3, 4, 5 — this is the assertion that fails if the
    // countup ever comes back.
    for (let i = 0; i < 4; i++) {
      await page.waitForTimeout(700);
      expect(
        (await cell.textContent())?.trim(),
        "Last synced ticked while nothing happened",
      ).toBe("seconds ago");
    }

    // NOT asserted here: the crossing to "1 minute ago", which is now
    // the only self-repaint this column does. Catching it means waiting
    // out a real minute, and this suite runs in about 40s — so the
    // repaint loop itself is covered only by the unit test on the text
    // it paints. If the tick regresses, a stale cell survives until the
    // next data change repaints the grid.

    // The stamp underneath is unchanged — the row is not re-syncing.
    expect(await lastSyncedOf(page, "pdfs/ingest")).toBe(stamp);

    // A row that never ran has no time to be relative to, and nothing
    // to reveal. `unsynced/ingest` exists in the config for exactly this:
    // the data root is shared by every test in this file, so any step
    // one of them syncs would make this order-dependent.
    await expect(
      row(page, "unsynced/ingest").locator('[col-id="lastSynced"]'),
    ).toHaveText("—");
    expect(await lastSyncedOf(page, "unsynced/ingest")).toBeNull();
  });

  test("sorting Last synced orders by time, not by how the cell reads", async ({
    page,
  }) => {
    // The column shows "5 minutes ago" and sorts on the underlying
    // stamp. Those two orders genuinely disagree here, which is what
    // makes this worth asserting through the real header rather than
    // only against the comparator: alphabetically "1 hour ago" precedes
    // "seconds ago", while chronologically it follows it.
    await writeConfigAndOpenGroups(page, config());

    // Two rows with a real gap between them, so the orders differ. Each
    // sync must be finished before the next begins, or the stamps can
    // land in either order — which is the thing being sorted.
    const sortWas = await stampsBefore(page, ["docs/ingest", "pdfs/ingest", "pdfs/render_markdown"]);
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
    /// `ag-row-level-N` class AG Grid puts on every row.
    type Seen = { id: string; level: number; stamp: string | null };
    const ordering = async (): Promise<Seen[]> =>
      page.locator(".ag-grid-scrolling-rows .ag-row").evaluateAll((rows) =>
        rows
          .sort(
            (a, b) =>
              Number((a as HTMLElement).getAttribute("aria-rowindex")) -
              Number((b as HTMLElement).getAttribute("aria-rowindex")),
          )
          .map((r) => ({
            id: r.getAttribute("row-id") ?? "",
            level: Number(/ag-row-level-(\d+)/.exec(r.className)?.[1] ?? "0"),
            stamp:
              r.querySelector('[col-id="lastSynced"] [title]')?.getAttribute("title") ?? null,
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

    const header = page.locator('.ag-header-cell[col-id="lastSynced"]');

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

  test("a downstream step can't be synced on its own, and says what would carry it", async ({
    page,
  }) => {
    await writeConfigAndOpenGroups(page, config());

    // `datalib-dag` rejects a `--sync` naming anything but a source
    // step, so this button would only ever queue a job that fails on
    // startup. It is disabled, and names the row that does carry it.
    const btn = syncBtn(page, "pdfs/render_markdown");
    await expect(btn).toBeDisabled();
    await expect(btn).toHaveAttribute("title", /Run pdfs\/ingest/);

    // A source step, by contrast, is runnable.
    await expect(syncBtn(page, "pdfs/ingest")).toBeEnabled();
  });
});
