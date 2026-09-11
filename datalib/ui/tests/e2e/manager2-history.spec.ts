// Right-click a row on the Manage screen → "Show commit history" → the
// store's dolt_log as rows. The fixture root's grid index is a real
// doltlite store with real commits, so the rows here are read from it.

import { test, expect } from "@playwright/test";
import { expectGridPainted } from "./grid-helpers";

const ROWS = '.ag-grid-scrolling-rows [role="row"]';

test("a group's commit history opens from the context menu", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  const groupRow = page.locator(ROWS).filter({ hasText: "Unified Index" }).first();
  await expect(groupRow).toBeVisible({ timeout: 10_000 });

  await groupRow.click({ button: "right" });
  await page.getByText("Show commit history").click();

  const dialog = page.getByRole("dialog", { name: "Commit history" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole("heading")).toHaveText(/Unified Index — commit history/);

  // The grid index store has at least the one commit that loaded the
  // fixture's rows, and that commit put rows in `grid_rows`.
  const rows = dialog.locator(ROWS);
  await expect(rows).not.toHaveCount(0, { timeout: 10_000 });
  // Geometry, not just DOM: the modal's grid is a second AG Grid under a
  // flex parent, the shape that has collapsed to 2px in WebKit before.
  await expectGridPainted(dialog.locator(".ag-root-wrapper"), "commit history grid");
  await expect(dialog.locator('.ag-cell[col-id="store"]').first()).toHaveText(
    "db.doltlite_db",
  );
  await expect(dialog.locator('.ag-cell[col-id="tables"]').first()).toContainText(
    "grid_rows",
  );
  // The hash cell carries the copy button, titled with the full hash.
  await expect(dialog.locator(".m2-copy-id").first()).toHaveAttribute(
    "title",
    /\(([0-9a-f]{40})\)$/,
  );

  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("an applet row offers no history", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  const groupRow = page.locator(ROWS).filter({ hasText: "Unified Index" }).first();
  await expect(groupRow).toBeVisible({ timeout: 10_000 });
  // Open the group so its applet row is on screen.
  await groupRow.locator(".ag-group-contracted").click();
  // The applet is the one child whose step-role mark says so.
  const appletRow = page.locator(ROWS).filter({ has: page.locator('[title="Applet"]') });
  await expect(appletRow).toBeVisible();

  await appletRow.click({ button: "right" });
  await expect(page.getByText("Show commit history")).toHaveCount(0);
});
