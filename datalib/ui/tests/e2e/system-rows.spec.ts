// The Manage card's last group is one the config never named: System,
// which is `system/` — with Logs under it, the run log. Both weigh
// something; the log is what Browse opens; nothing here syncs, edits
// or removes.

import { test, expect } from "@playwright/test";
import { MENU_DISABLED, expandRow, pipelineRow, rowMenuEntry } from "./grid-helpers";

test("System and its Logs child: sizes, a Browse that opens the log, no Sync", async ({
  page,
}) => {
  await page.goto("/data_sources");
  const system = pipelineRow(page, "system");
  await expect(system).toBeVisible({ timeout: 10_000 });
  await expect(system.locator('[col-id="name"]')).toContainText("System");
  await expect(system.getByRole("button", { name: "Sync now" })).toBeDisabled();

  await expandRow(system, "group system");
  const logs = pipelineRow(page, "system/runs.sqlite");
  await expect(logs).toBeVisible();
  await expect(logs.locator('[col-id="name"]')).toContainText("Logs");
  // The run store exists on a served root, so both rows carry a size.
  await expect(system.locator('[col-id="disk"]')).not.toHaveText(/^\s*[—-]?\s*$/);
  await expect(logs.locator('[col-id="disk"]')).not.toHaveText(/^\s*[—-]?\s*$/);

  // Not a config entry: the menu says so where an entry would edit it.
  const remove = await rowMenuEntry(page, logs, /^Remove/).open();
  await expect(remove).toHaveClass(MENU_DISABLED);
  await expect(remove.locator(".slick-menu-content")).toHaveAttribute("title", "Not a config entry");
  await page.keyboard.press("Escape");

  await logs.getByRole("button", { name: "Browse the log" }).click();
  const col = page.locator(".miller-col").filter({ has: page.locator(".rl-panel") });
  await expect(col).toBeVisible({ timeout: 10_000 });
  await expect(col.locator(".miller-col-title")).toHaveText("Log · everything");
});
