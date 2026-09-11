// The Manage screen's right-click menu: every row action in one place,
// with Lightroom selection semantics, and a rename done in the cell.
// Config-mutating (the rename writes config.toml), so it runs against a
// sandbox root of its own — see `config-mutating.ts`.

import { test, expect, type Page } from "@playwright/test";
import { expandGroup, groupRow, pipelineRow } from "./grid-helpers";

const menuEntries = (page: Page) => page.locator(".ag-menu-option .ag-menu-option-text");

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
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
    "Edit settings…",
    "Show log",
    "Show commit history",
    "Remove from config, with everything under it",
  ]);
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
  await expect(page.locator(".ag-row-selected")).toHaveCount(2);

  await qmd.locator('[col-id="status"]').click({ button: "right" });
  await expect(menuEntries(page).last()).toHaveText("Remove 2 entries from config");
  // The one-row actions say so, and a reason names the row it came from.
  const history = page.locator(".ag-menu-option", { hasText: "Show commit history" });
  await expect(history).toHaveClass(/ag-menu-option-disabled/);
  await history.hover();
  await expect(page.getByText("QMD index: The QMD index keeps no doltlite store")).toBeVisible();
  await page.keyboard.press("Escape");

  // A row outside the selection becomes the whole selection.
  const group = groupRow(page, "unified_index");
  await group.locator('[col-id="status"]').click({ button: "right" });
  await expect(page.locator(".ag-row-selected")).toHaveCount(1);
  await expect(menuEntries(page).last()).toHaveText(
    "Remove from config, with everything under it",
  );
  await page.keyboard.press("Escape");
});

test("Rename edits the group's name in the cell and writes it to the config", async ({
  page,
}) => {
  await openManager(page);
  const editor = page.locator(".m2-editor");
  const original = await editor.inputValue();
  const row = groupRow(page, "unified_index");
  await expect(row).toBeVisible({ timeout: 10_000 });

  await row.locator('[col-id="name"]').click({ button: "right" });
  await page.locator(".ag-menu-option", { hasText: "Rename…" }).click();
  const input = page.locator(".ag-cell-inline-editing input");
  await expect(input).toBeVisible();
  await input.fill("Everything, indexed");
  await input.press("Enter");

  await expect(page.getByText("Renamed unified_index to Everything, indexed.")).toBeVisible();
  await expect(row.locator(".m2-group-name")).toHaveText("Everything, indexed");
  await expect(editor).toHaveValue(/name = "Everything, indexed"/);

  // Put the root back for the next spec on this sandbox.
  await page.getByText("Advanced — edit config.toml directly").click();
  await editor.fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});
