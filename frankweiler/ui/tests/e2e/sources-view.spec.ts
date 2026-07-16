// The merged Sources tab: the sources table and the raw config.yaml
// editor sit side by side as two views of the same text. The editor is
// the single source of truth; the table re-derives from it on every
// keystroke. A row's Edit button selects that source's stanza in the
// editor; the chips append a template stanza. Save PUTs the text to
// /api/config, which validates with the real config loader before
// persisting.
//
// The fixture root's config.yaml starts as a single `data_root:` line
// with no sources. Specs that save restore the original file at the end
// (the fixture root is shared by every spec in the run).

import { test, expect, type Page } from "@playwright/test";

async function openSources(page: Page) {
  await page.goto("/sources");
  await expect(page.getByRole("heading", { name: "Configure data sources" })).toBeVisible();
}

test("add a source via chip, save, sync lights up, restore", async ({ page }) => {
  await openSources(page);

  // Fixture config has no sources; the editor holds the raw file.
  await expect(page.getByText("no sources configured yet")).toBeVisible();
  const editor = page.locator(".editor");
  await expect(editor).toHaveValue(/data_root:/);
  const original = await editor.inputValue();

  // The chip appends a stanza to the text; the table row appears
  // immediately (derived from the text), but stays unsyncable until
  // the config is saved.
  await page.getByRole("button", { name: "+ Perseus (sample)" }).click();
  await expect(editor).toHaveValue(/type: perseus/);
  const row = page.locator(".sources-table tbody tr", { hasText: "perseus" }).first();
  await expect(row).toContainText("perseus");
  await expect(page.getByText("unsaved changes")).toBeVisible();
  await expect(row.getByRole("button", { name: "Sync", exact: true })).toBeDisabled();

  // Save: the backend validates with the real config loader and
  // persists; the row's Sync button lights up.
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 1 source(s) configured.")).toBeVisible();
  await expect(row.getByRole("button", { name: "Sync", exact: true })).toBeEnabled();

  // Restore the original config so later specs see the fixture unchanged.
  await editor.fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 0 source(s) configured.")).toBeVisible();
  await expect(page.getByText("no sources configured yet")).toBeVisible();
});

test("Edit selects the source's stanza in the editor", async ({ page }) => {
  await openSources(page);

  const editor = page.locator(".editor");
  await page.getByRole("button", { name: "+ Perseus (sample)" }).click();
  await page.getByRole("button", { name: "+ ChatGPT" }).click();

  // Select the first source; the selection must cover exactly its stanza.
  await page
    .locator(".sources-table tbody tr", { hasText: "perseus" })
    .getByRole("button", { name: "Edit" })
    .click();
  const selected = await editor.evaluate((el) => {
    const t = el as HTMLTextAreaElement;
    return t.value.slice(t.selectionStart, t.selectionEnd);
  });
  expect(selected).toContain("- name: perseus");
  expect(selected).toContain("type: perseus");
  expect(selected).not.toContain("chatgpt");

  // Unsaved edits never reached the server; a reload restores the file.
  await page.reload();
  await expect(page.locator(".editor")).not.toHaveValue(/perseus/);
});

test("a YAML error marks the table stale instead of blanking it", async ({ page }) => {
  await openSources(page);

  const editor = page.locator(".editor");
  const original = await editor.inputValue();

  await editor.fill(original + "\nbroken: [unclosed\n");
  await expect(page.getByText(/config has a YAML error/)).toBeVisible();
  // Save is still possible but the backend rejects it.
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText(/✗ Not saved:/)).toBeVisible();

  // Fixing the text clears the error.
  await editor.fill(original);
  await expect(page.getByText(/config has a YAML error/)).not.toBeVisible();
});

test("invalid config is rejected by Save and not persisted", async ({ page }) => {
  await openSources(page);

  const editor = page.locator(".editor");
  const original = await editor.inputValue();

  // Parses as YAML but fails the config loader (unknown source type).
  await editor.fill("sources:\n  - name: x\n    source: {type: not_a_provider}\n");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText(/✗ Not saved:/)).toBeVisible();

  // The file on disk is unchanged.
  await page.reload();
  await expect(page.locator(".editor")).toHaveValue(original);
});
