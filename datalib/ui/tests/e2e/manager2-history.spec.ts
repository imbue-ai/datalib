// Right-click a row on the Manage screen → "Show commit history" → the
// store's dolt_log as a tree: the store, its commits, and under each
// commit the tables it left behind. The fixture root's grid index is a
// real doltlite store with real commits, so the rows here are read from
// it.

import { test, expect } from "@playwright/test";
import { expandGroup, expectGridPainted, groupRow } from "./grid-helpers";

const ROWS = '.ag-grid-scrolling-rows [role="row"]';

test("a group's commit history opens from the context menu as a tree", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.click({ button: "right" });
  await page.getByText("Show commit history").click();

  const dialog = page.getByRole("dialog", { name: "Commit history" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole("heading")).toHaveText(/Unified Index — commit history/);

  const rows = dialog.locator(ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  // Geometry, not just DOM: the modal's grid is a second AG Grid under a
  // flex parent, the shape that has collapsed to 2px in WebKit before.
  await expectGridPainted(dialog.locator(".ag-root-wrapper"), "commit history grid");

  // The store leads, open, with its commits under it.
  const store = rows.filter({ has: page.locator(".m2-history-store") }).first();
  await expect(store).toContainText("db.doltlite_db");
  const commit = rows.filter({ hasText: "datalib-step grid_index" }).first();
  await expect(commit.locator(".m2-copy-id")).toHaveAttribute("title", /\(([0-9a-f]{40})\)$/);
  // The fixture is built by `datalib-dag`, so its commits carry the run
  // that made them.
  await expect(commit.locator(".m2-history-run")).toBeVisible();

  // Opening a commit shows what it did to each table.
  await commit.locator(".ag-group-contracted:not(.ag-hidden)").click();
  await expect(rows.filter({ has: page.locator(".m2-history-table") }).first()).toContainText(
    "grid_rows",
  );

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("an applet row keeps the entry, disabled, with the reason", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  await expandGroup(page, "unified_index");
  const appletRow = page.locator(ROWS).filter({ has: page.locator('[title="Applet"]') });
  await expect(appletRow).toBeVisible();

  await appletRow.click({ button: "right" });
  const entry = page.locator(".ag-menu-option", { hasText: "Show commit history" });
  await expect(entry).toHaveClass(/ag-menu-option-disabled/);
  await entry.hover();
  await expect(page.getByText("An applet writes no store")).toBeVisible();
});
