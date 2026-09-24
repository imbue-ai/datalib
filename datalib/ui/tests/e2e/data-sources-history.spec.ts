// Right-click a row on the Manage screen → "Show commit history" → the
// store's dolt_log as a tree: the store, its commits, and under each
// commit the tables it left behind. The fixture root's grid index is a
// real doltlite store with real commits, so the rows here are read from
// it.

import { test, expect } from "@playwright/test";
import {
  expandGroup,
  expectGridPainted,
  groupRow,
  MENU_DISABLED,
  TABLE_ROWS,
  menuEntry,
} from "./grid-helpers";

const ROWS = TABLE_ROWS;

test("a group's commit history opens from the context menu as a tree", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.click({ button: "right" });
  await page.getByText("Show commit history").click();

  const dialog = page.getByRole("dialog", { name: "Commit history" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole("heading")).toHaveText(/Unified Index — commit history/);

  const rows = dialog.locator(ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  // Geometry, not just DOM: the modal's grid is a second grid under a
  // flex parent, the shape that has collapsed to 2px in WebKit before.
  await expectGridPainted(dialog.locator(".slickgrid-container"), "commit history grid");

  // The store leads, open, with its commits under it.
  const store = rows.filter({ has: page.locator(".m2-history-store") }).first();
  await expect(store).toContainText("db.doltlite_db");
  const commit = rows.filter({ hasText: "datalib-step grid_index" }).first();
  await expect(commit.locator(".m2-copy-id")).toHaveAttribute("title", /\(([0-9a-f]{40})\)$/);
  // The fixture is built by `datalib-dag`, so its commits carry the run
  // that made them.
  await expect(commit.locator(".m2-history-run")).toBeVisible();

  // Opening a commit shows what it did to each table.
  await commit.locator(".slick-tree-toggle.collapsed").click();
  const table = rows.filter({ has: page.locator(".m2-history-table") }).first();
  await expect(table).toContainText("grid_rows");

  // A refresh that fails leaves the log on screen, and the commit
  // opened in it open. The tab coming back to the foreground is a
  // resync, which re-reads an open history.
  await table.evaluate((el) => el.setAttribute("data-probe", ""));
  await page.route("**/api/pipeline/history**", (r) =>
    r.fulfill({ status: 500, contentType: "text/plain", body: "store busy" }),
  );
  await page.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));
  await expect(dialog.getByText(/The last refresh failed/)).toBeVisible();
  await expect(
    rows.and(page.locator("[data-probe]")),
    "a failed refresh took the history grid down",
  ).toBeVisible();
  await page.unroute("**/api/pipeline/history**");

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("an applet row keeps the entry, disabled, with the reason", async ({ page }) => {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await expandGroup(page, "unified_index");
  const appletRow = page.locator(ROWS).filter({ has: page.locator('[title="Applet"]') });
  await expect(appletRow).toBeVisible();

  await appletRow.click({ button: "right" });
  const entry = menuEntry(page, "Show commit history");
  await expect(entry).toHaveClass(MENU_DISABLED);
  // The reason is the entry's own hover text.
  await expect(entry.locator(".slick-menu-content")).toHaveAttribute(
    "title",
    /An applet writes no store/,
  );
});
