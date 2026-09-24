// The Manage screen's right-click menu: every row action in one place,
// with Lightroom selection semantics, and a rename done in the cell.
// Config-mutating (the rename writes config.toml), so it runs against a
// sandbox root of its own — see `config-mutating.ts`.

import { test, expect, type Page } from "@playwright/test";
import {
  MENU_DISABLED,
  SELECTED_ROWS,
  expandGroup,
  groupRow,
  menuEntry,
  pipelineRow,
  MANAGE_WITH_CONFIG,
} from "./grid-helpers";

/// The menu's entries by their text — separators carry none.
const menuEntries = (page: Page) => page.locator(".slick-context-menu .slick-menu-content");

async function openManager(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
}

test("a row's menu offers every action, and the cell under the pointer adds its own", async ({
  page,
}) => {
  await openManager(page);
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.locator('[col-id="status"]').click({ button: "right" });
  await expect(menuEntries(page)).toHaveText([
    "Browse every source",
    "Sync now",
    "Pause",
    "Edit settings…",
    "Compare two syncs…",
    "Show log",
    "Show commit history",
    "Reset (preserve attachments)…",
    "Reset (drop attachments)…",
    "Remove from config, with everything under it",
  ]);
  // The index rebuilds from the sources, so it is not reset by hand.
  await expect(menuEntry(page, "Reset (preserve attachments)…")).toHaveClass(MENU_DISABLED);
  await page.keyboard.press("Escape");

  // The Name cell adds Rename and Copy id ahead of the row's entries.
  await row.locator('[col-id="name"]').click({ button: "right" });
  await expect(menuEntries(page).first()).toHaveText("Rename…");
  await expect(menuEntries(page).nth(1)).toHaveText("Copy id");
  await page.keyboard.press("Escape");
});

test("right-clicking inside a selection targets all of it; outside it, the one row", async ({
  page,
}) => {
  await openManager(page);
  await expandGroup(page, "unified_index");
  const grid = pipelineRow(page, "unified_index/grid_index");
  const qmd = pipelineRow(page, "unified_index/qmd_index");
  await expect(grid).toBeVisible();
  await grid.locator('[col-id="status"]').click();
  await qmd.locator('[col-id="status"]').click({ modifiers: ["ControlOrMeta"] });
  await expect(page.locator(SELECTED_ROWS)).toHaveCount(2);

  await qmd.locator('[col-id="status"]').click({ button: "right" });
  await expect(menuEntries(page).last()).toHaveText("Remove 2 entries from config");
  // The one-row actions say so, and a reason names the row it came from.
  const history = menuEntry(page, "Show commit history");
  await expect(history).toHaveClass(MENU_DISABLED);
  await expect(history.locator(".slick-menu-content")).toHaveAttribute(
    "title",
    /QMD index: The QMD index keeps no doltlite store/,
  );
  await page.keyboard.press("Escape");

  // A row outside the selection is the one target, and the selection
  // is left exactly as it was — a right-click aims, it does not select.
  const group = groupRow(page, "unified_index");
  await group.locator('[col-id="status"]').click({ button: "right" });
  await expect(menuEntries(page).last()).toHaveText("Remove from config, with everything under it");
  await expect(page.locator(SELECTED_ROWS)).toHaveCount(2);
  await expect(group.locator(".slick-cell.selected")).toHaveCount(0);
  await page.keyboard.press("Escape");
});

test("Rename edits the group's name in the cell and writes it to the config", async ({ page }) => {
  await openManager(page);
  const editor = page.locator(".m2-editor");
  const original = await editor.inputValue();
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.locator('[col-id="name"]').click({ button: "right" });
  await menuEntry(page, "Rename…").click();
  const input = page.locator(".tg-grid input.editor-text");
  await expect(input).toBeVisible();
  // The table repaints cells on a clock ("12 seconds ago" goes stale),
  // and a repaint of the cell being edited would reset it under the
  // typist. Force one of this very column and expect the same input to
  // survive it.
  await page
    .locator(".tg-grid")
    .first()
    .evaluate((el) => {
      (el as HTMLElement & { __api: { refreshCells(f: string[]): void } }).__api.refreshCells([
        "name",
      ]);
    });
  await page.waitForTimeout(200);
  await expect(input).toBeVisible();
  await expect(input).toBeFocused();
  await input.fill("Everything, indexed");
  await input.press("Enter");

  await expect(page.getByText("Renamed unified_index to Everything, indexed.")).toBeVisible();
  await expect(row.locator(".tg-parent")).toHaveText("Everything, indexed");
  await expect(editor).toHaveValue(/name = "Everything, indexed"/);

  // Put the root back for the next spec on this sandbox.
  await editor.fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});
