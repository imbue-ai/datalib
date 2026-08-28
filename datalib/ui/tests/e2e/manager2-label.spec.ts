// Manager2: a source's label is editable, its name is not.
//
// The name is the stanza directory, the step-id stem, and the prefix
// the grid index already recorded in every `qmd_path` — so the wizard
// holds it read-only and `label` is the half a person can rewrite. This
// spec walks that end to end: add a source with a label, see the grid
// show the label with the directory name muted beside it, reopen Edit
// and find the label filled and the name disabled, then clear it.
//
// The fixture root's config.toml starts with no sources and is shared
// across the whole run, so this spec restores it before finishing.
import { test, expect, type Page } from "@playwright/test";

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Data sources" })).toBeVisible();
}

const wizard = (page: Page) => page.getByRole("dialog");
// Located structurally rather than by accessible name: each field's
// `<label>` wraps its help paragraph too, so the accessible name is the
// caption plus a sentence of prose.
const field = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) > .wiz-input`);
const labelField = (page: Page) => field(page, "Label");
const nameField = (page: Page) => field(page, "Name");

// Captured before the test edits it and put back afterwards even when
// the test fails: the data root is one mkdtemp shared by every spec in
// the run, so a failure that left a source behind would cascade.
let original = "";

test.beforeEach(async ({ page }) => {
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});

test("a label is editable, the name is not, and clearing it drops the key", async ({ page }) => {
  const editor = page.locator(".m2-editor");

  // --- Add, with a label -------------------------------------------
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  // By blurb: "Claude" alone also matches the "Claude export" tile.
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror your claude.ai conversations" })
    .click();

  // The name is proposed from the catalog; the label is empty and
  // shows the name as its placeholder, so a source with no label is
  // still displayed by its directory.
  await expect(nameField(page)).toHaveValue("claude");
  await expect(labelField(page)).toHaveValue("");
  await expect(labelField(page)).toHaveAttribute("placeholder", "claude");

  await labelField(page).fill("Personal Claude");
  // The TOML preview is the wizard's own honesty check: the label rides
  // on the download step, and the name is untouched by it.
  await wizard(page).getByText("Review the TOML this writes").click();
  const preview = wizard(page).locator(".wiz-review pre");
  await expect(preview).toContainText('label = "Personal Claude"');
  await expect(preview).toContainText('outputs = ["claude/raw"]');

  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added claude.")).toBeVisible();

  // --- The grid shows the label, and still names the directory ------
  const row = page.locator(".ag-row", { hasText: "Personal Claude" });
  await expect(row).toBeVisible();
  await expect(row.locator(".m2-cell-dir")).toHaveText("claude");
  // It really is in the file, on the download step.
  await expect(editor).toHaveValue(/label = "Personal Claude"/);

  // --- Edit: label filled and editable, name fixed ------------------
  await row.getByRole("button", { name: "Edit" }).click();
  await expect(labelField(page)).toHaveValue("Personal Claude");
  await expect(nameField(page)).toBeDisabled();
  await expect(wizard(page)).toContainText("Use Label above for a name you can change.");

  // --- Clearing the label removes the key, and the row falls back ---
  await labelField(page).fill("");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved claude.")).toBeVisible();
  await expect(editor).not.toHaveValue(/label = /);
  await expect(page.locator(".ag-row", { hasText: "Personal Claude" })).toHaveCount(0);
  await expect(page.locator(".ag-row").first()).toContainText("claude");
});
