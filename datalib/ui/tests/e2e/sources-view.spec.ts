// The merged Sources tab: the sources table and the raw config.yaml
// editor sit side by side as two views of the same text. The editor is
// the single source of truth; the table re-derives from it on every
// keystroke. A row's "Locate config" button selects that stanza in the
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

  // The chip appends the source's step pair to the text; the table
  // row appears immediately (derived from the text), but stays
  // unsyncable — checkbox disabled — until the config is saved.
  await page.getByRole("button", { name: "Perseus (sample)" }).click();
  await expect(editor).toHaveValue(/command: datalib-step download perseus/);
  const row = page.locator(".sources-table tbody tr", { hasText: "perseus" }).first();
  await expect(row).toContainText("perseus");
  await expect(page.getByText("unsaved changes")).toBeVisible();
  const checkbox = row.locator("input[type=checkbox]");
  await expect(checkbox).toBeDisabled();

  // Save: the backend validates with the runner's real config chain
  // and persists; the row becomes selectable and "Sync selected"
  // lights up once it's checked.
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 1 source(s) configured.")).toBeVisible();
  await expect(checkbox).toBeEnabled();
  await checkbox.check();
  const syncSelected = page.getByRole("button", { name: /Sync selected \(1\)/ });
  await expect(syncSelected).toBeEnabled();

  // Any unsaved edit re-blocks sync (even for saved rows): sync runs
  // against the file on disk, which no longer matches the editor.
  // Collapse the caret to the end first — the chip left its steps
  // selected, and typing over the selection would replace them.
  await editor.evaluate((el) => {
    const t = el as HTMLTextAreaElement;
    t.focus();
    t.setSelectionRange(t.value.length, t.value.length);
  });
  await editor.pressSequentially(" ");
  await expect(syncSelected).toBeDisabled();

  // Restore the original config so later specs see the fixture unchanged.
  await editor.fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("✓ Saved — 0 source(s) configured.")).toBeVisible();
  await expect(page.getByText("no sources configured yet")).toBeVisible();
});

test("Locate config selects the source's stanza in the editor", async ({ page }) => {
  await openSources(page);

  const editor = page.locator(".editor");
  await page.getByRole("button", { name: "Perseus (sample)" }).click();
  await page.getByRole("button", { name: "ChatGPT" }).click();

  // Select the first source; the selection must cover exactly its
  // step entry (the input-less download step — the render step has
  // inputs and is not a source row).
  await page
    .locator(".sources-table tbody tr", { hasText: "perseus" })
    .getByRole("button", { name: "Locate config" })
    .click();
  const selected = await editor.evaluate((el) => {
    const t = el as HTMLTextAreaElement;
    return t.value.slice(t.selectionStart, t.selectionEnd);
  });
  expect(selected).toContain("id: perseus.download");
  expect(selected).not.toContain("id: perseus.render");
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
  // The parse error and the unsaved-changes note are independent — both
  // show at once.
  await expect(page.getByText(/YAML error \(table may be stale\)/)).toBeVisible();
  await expect(page.getByText("unsaved changes")).toBeVisible();
  // Save is still possible but the backend rejects it.
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText(/✗ Not saved:/)).toBeVisible();

  // Fixing the text clears the error.
  await editor.fill(original);
  await expect(page.getByText(/YAML error \(table may be stale\)/)).not.toBeVisible();
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
