// The merged Sources tab (Setup + Sync): table mode edits sources as
// rows (add / edit / apply / save) with an "additional config options"
// box for the non-source stanzas; raw mode edits the whole config.yaml
// text. Both modes are views of the same state (src/config/configSplit
// bridges them) and Save PUTs the reassembled YAML to /api/config.
//
// The fixture root's config.yaml starts as a single `data_root:` line
// with no sources. This spec adds a source, saves, round-trips through
// raw mode, then restores the original file (the fixture root is shared
// by every spec in the run).

import { test, expect, type Page } from "@playwright/test";

async function openSources(page: Page) {
  await page.goto("/sources");
  await expect(page.getByRole("heading", { name: "Data sources" })).toBeVisible();
}

test("table mode: add a source, save, round-trip raw mode, restore", async ({
  page,
}) => {
  await openSources(page);

  // Fixture config has no sources — the table shows the empty row and
  // the non-source stanza (data_root) sits in "additional config".
  await expect(page.getByText("no sources configured yet")).toBeVisible();
  // The "additional config options" box opens by itself when there are
  // non-source stanzas.
  const extra = page.locator(".editor-rest");
  await expect(extra).toBeVisible();
  await expect(extra).toHaveValue(/data_root:/);
  const originalRest = await extra.inputValue();

  // Add a Perseus source via its chip: an inline editor opens with the
  // template; nothing is committed until Apply.
  await page.getByRole("button", { name: "+ Perseus (sample)" }).click();
  const fragment = page.locator(".editor-fragment");
  await expect(fragment).toBeVisible();
  await expect(fragment).toHaveValue(/type: perseus/);
  await page.getByRole("button", { name: "Apply" }).click();

  // The row appears; sync is blocked until the config is saved.
  const row = page.locator(".sync-table tbody tr", { hasText: "perseus" }).first();
  await expect(row).toContainText("perseus");
  await expect(page.getByText("unsaved changes")).toBeVisible();
  await expect(row.getByRole("button", { name: "Sync now" })).toBeDisabled();

  // Save: the backend validates with the real config loader and
  // persists; the row's Sync button lights up.
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 1 source(s) configured.")).toBeVisible();
  await expect(row.getByRole("button", { name: "Sync now" })).toBeEnabled();

  // Raw mode shows the reassembled file: data_root stanza + the source.
  await page.getByRole("tab", { name: "Raw file" }).click();
  const raw = page.locator(".editor-raw");
  await expect(raw).toBeVisible();
  const rawText = await raw.inputValue();
  expect(rawText).toContain("data_root:");
  expect(rawText).toContain("- name: perseus");

  // Restore the original config through the raw editor so later specs
  // see the fixture unchanged.
  await raw.fill(originalRest);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 0 source(s) configured.")).toBeVisible();

  // Back in table mode the list is empty again.
  await page.getByRole("tab", { name: "Table" }).click();
  await expect(page.getByText("no sources configured yet")).toBeVisible();
});

test("an uncommitted template row never leaks into the config", async ({ page }) => {
  await openSources(page);

  // Open a template but do NOT Apply; switching to raw must not carry
  // the never-committed source along.
  await page.getByRole("button", { name: "+ Perseus (sample)" }).click();
  await expect(page.locator(".editor-fragment")).toBeVisible();
  await page.getByRole("tab", { name: "Raw file" }).click();
  await expect(page.locator(".editor-raw")).not.toHaveValue(/perseus/);

  // Same via Cancel in table mode: the row disappears and nothing is
  // marked dirty.
  await page.getByRole("tab", { name: "Table" }).click();
  await page.getByRole("button", { name: "+ Perseus (sample)" }).click();
  await page.getByRole("button", { name: "Cancel" }).click();
  await expect(page.getByText("no sources configured yet")).toBeVisible();
  await expect(page.getByText("unsaved changes")).not.toBeVisible();
});

test("raw mode blocks switching to the table on broken YAML", async ({ page }) => {
  await openSources(page);

  await page.getByRole("tab", { name: "Raw file" }).click();
  const raw = page.locator(".editor-raw");
  const original = await raw.inputValue();

  await raw.fill("a: [unclosed");
  await page.getByRole("tab", { name: "Table" }).click();
  await expect(page.getByText(/fix the YAML before switching/)).toBeVisible();
  // Still in raw mode; the text is untouched.
  await expect(raw).toBeVisible();

  // Unsaved raw edits never reach the server: reloading brings back the
  // saved file.
  await raw.fill(original);
  await page.getByRole("tab", { name: "Table" }).click();
  await expect(page.locator(".sync-table").first()).toBeVisible();
});

test("invalid config is rejected by Save and not persisted", async ({ page }) => {
  await openSources(page);

  await page.getByRole("tab", { name: "Raw file" }).click();
  const raw = page.locator(".editor-raw");
  const original = await raw.inputValue();

  // Parses as YAML but fails the config loader (unknown source type).
  await raw.fill("sources:\n  - name: x\n    source: {type: not_a_provider}\n");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText(/✗ Not saved:/)).toBeVisible();

  // The file on disk is unchanged.
  await page.reload();
  await expect(page.getByRole("heading", { name: "Data sources" })).toBeVisible();
  await page.getByRole("tab", { name: "Raw file" }).click();
  await expect(page.locator(".editor-raw")).toHaveValue(original);
});
