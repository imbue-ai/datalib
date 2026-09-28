// Right-click a row on the Manage screen → "Show commit history" → a
// card beside it with the store's dolt_log as a tree: the store, its
// commits, and under each commit the tables it left behind. The fixture root's grid index is a
// real doltlite store with real commits, so the rows here are read from
// it.

import { test, expect } from "@playwright/test";
import { expandGroup, expectGridPainted, groupRow, TABLE_ROWS, menuEntry } from "./grid-helpers";

const ROWS = TABLE_ROWS;

test("a group's commit history opens from the context menu as a card", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.click({ button: "right" });
  await page.getByText("Show commit history").click();

  const card = page.locator(".miller-col").filter({ has: page.locator(".hc") });
  await expect(card).toBeVisible({ timeout: 10_000 });
  await expect(card.locator(".miller-col-title")).toHaveText("History · Unified Index");
  // Not a source, so nothing here to compare.
  await expect(card.getByText(/right-click to compare/)).toHaveCount(0);

  const rows = card.locator(ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  // Geometry, not just DOM: the card's grid sits under a flex parent,
  // the shape that has collapsed to 2px in WebKit before.
  await expectGridPainted(card.locator(".slickgrid-container"), "commit history grid");

  // The store leads, open, with its commits under it.
  const store = rows.filter({ has: page.locator(".hc-store") }).first();
  await expect(store).toContainText("db.doltlite_db");
  const commit = rows.filter({ hasText: "datalib-step grid_index" }).first();
  await expect(commit.locator(".hc-copy-id")).toHaveAttribute("title", /\(([0-9a-f]{40})\)$/);
  // The fixture is built by `datalib-dag`, so its commits carry the run
  // that made them.
  await expect(commit.locator(".hc-run")).toBeVisible();

  // Opening a commit shows what it did to each table.
  await commit.locator(".slick-tree-toggle.collapsed").click();
  const table = rows.filter({ has: page.locator(".hc-table") }).first();
  await expect(table).toContainText("grid_rows");

  // A refresh that fails leaves the log on screen, and the commit
  // opened in it open. The tab coming back to the foreground is a
  // resync, which re-reads the history.
  await table.evaluate((el) => el.setAttribute("data-probe", ""));
  await page.route("**/api/pipeline/history**", (r) =>
    r.fulfill({ status: 500, contentType: "text/plain", body: "store busy" }),
  );
  await page.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));
  await expect(card.getByText(/The last refresh failed/)).toBeVisible();
  await expect(
    rows.and(page.locator("[data-probe]")),
    "a failed refresh took the history grid down",
  ).toBeVisible();
  await page.unroute("**/api/pipeline/history**");

  // The run link opens a log beside the history. Which lines it shows
  // depends on whether the run store knows the fixture's run, so only
  // the card is pinned.
  await commit.locator(".hc-run").click();
  await expect(page.locator(".miller-col-title").last()).toHaveText(/^Log/);
});

test("an applet row leaves out what an applet cannot do", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await expandGroup(page, "unified_index");
  const appletRow = page.locator(ROWS).filter({ has: page.locator('[title="Applet"]') });
  await expect(appletRow).toBeVisible();

  await appletRow.click({ button: "right" });
  // The menu is open — an entry every row has is showing — so the
  // missing one is left out, not yet to be drawn.
  await expect(menuEntry(page, "Copy id")).toBeVisible();
  await expect(menuEntry(page, "Show commit history")).toHaveCount(0);
});

/// "Compare two versions…" opens the same card, set up to compare. The
/// fixture root renders from a cached ingest and keeps no raw store, so
/// what it sets up is the reason there is nothing to compare yet.
test("Compare two versions opens the history ready to compare", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  const row = groupRow(page, "slack");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.click({ button: "right" });
  await menuEntry(page, "Compare two versions…").click();

  const card = page.locator(".miller-col").filter({ has: page.locator(".hc") });
  await expect(card).toBeVisible({ timeout: 10_000 });
  await expect(card.locator(".hc-compare")).toContainText("This source has no synced data yet.");
  await card.getByRole("button", { name: "Cancel" }).click();
  await expect(card.locator(".hc-compare")).toHaveCount(0);
});

/// Creating a comparison writes it to the config and asks for its first
/// sync. The fixture's Slack has no download step, so the history is
/// served here with two commits of one, and the save and the sync are
/// answered here too: the server would refuse a comparison of a source
/// with no download, and a sync would render commits that do not exist.
test("Compare two versions writes a comparison of the two commits and syncs it", async ({
  page,
}) => {
  const commit = (hash: string, date: string) => ({
    hash,
    parent: null,
    committer: "doltlite",
    date,
    message: `sync ${hash.slice(0, 4)}`,
    run: null,
    tables: [],
  });
  await page.route("**/api/pipeline/history?tree=slack", (r) =>
    r.fulfill({
      json: {
        tree: "slack",
        stores: [
          {
            path: "slack/ingest/entities.doltlite_db",
            truncated: false,
            commits: [
              commit("b".repeat(32), "2026-09-02T10:00:00+00:00"),
              commit("a".repeat(32), "2026-09-01T10:00:00+00:00"),
            ],
          },
        ],
      },
    }),
  );
  const saved: string[] = [];
  await page.route("**/api/config", async (r) => {
    if (r.request().method() !== "PUT") return r.fallback();
    saved.push((r.request().postDataJSON() as { text: string }).text);
    await r.fulfill({ json: { ok: true, diagnostics: [], source_count: 25 } });
  });
  const asked: string[][] = [];
  await page.route("**/api/requests", async (r) => {
    if (r.request().method() !== "POST") return r.fallback();
    asked.push((r.request().postDataJSON() as { roots: string[] }).roots);
    await r.fulfill({ json: { id: "e2e-request", roots: [] } });
  });

  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  const row = groupRow(page, "slack");
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  await menuEntry(page, "Compare two versions…").click();
  const compare = page.locator(".hc-compare");
  // The newest two, older first.
  await expect(compare).toContainText(/aaaaaaaa.*→.*bbbbbbbb/);
  await compare.locator(".hc-name").fill("Slack e2e changes");
  await compare.getByRole("button", { name: "Create comparison" }).click();

  await expect(compare).toHaveCount(0);
  expect(saved).toHaveLength(1);
  expect(saved[0]).toContain('id = "slack-e2e-changes"');
  expect(saved[0]).toContain('source = "slack"');
  expect(saved[0]).toContain(`from = "${"a".repeat(32)}"`);
  expect(saved[0]).toContain(`to = "${"b".repeat(32)}"`);
  expect(asked).toEqual([["slack-e2e-changes/render_markdown"]]);
});
