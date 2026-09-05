// A path field in the Add Data Source wizard owes the user a native
// picker — see docs/dev/wizard_file_pickers.md. This suite runs in a
// plain Chromium, which is the ONE host that cannot have one: the
// browser never hands back a filesystem path, and the path the config
// needs is one on the machine running the backend anyway.

import { test, expect } from "@playwright/test";

test("a path field types in a browser and offers no dead picker button", async ({ page }) => {
  await page.goto("/sources2");
  await page.getByRole("button", { name: "+ Add Data Source" }).click();

  // WhatsApp is the descriptor that prompted the rule: one required
  // folder, which the user has open in Finder while they type it.
  await page.getByRole("searchbox").fill("whatsapp");
  await page.getByRole("button", { name: /WhatsApp/ }).click();

  const wizard = page.getByRole("dialog");
  // The footer repeats the label in "Still needed: …", so match the
  // field's own label rather than any text node holding the words.
  await expect(wizard.locator(".wiz-label", { hasText: "WhatsApp folder" })).toBeVisible();

  // No picker in a browser — not a disabled one, not a broken one.
  await expect(wizard.getByRole("button", { name: /Choose (folder|file)/ })).toHaveCount(0);

  // And the typed path still reaches the generated TOML, which is the
  // only way in that this host has.
  const pathInput = wizard.locator("input.wiz-path");
  await pathInput.fill("/Users/x/backups/WhatsApp");
  await wizard.getByText("Review the TOML this writes").click();
  await expect(wizard.locator("pre")).toContainText('backup_dir = "/Users/x/backups/WhatsApp"');

  // Required-field gating still applies to the field the picker feeds.
  await expect(wizard.getByRole("button", { name: "Add source" })).toBeEnabled();
  await pathInput.fill("");
  await expect(wizard.getByRole("button", { name: "Add source" })).toBeDisabled();
});
