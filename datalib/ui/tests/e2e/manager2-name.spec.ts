// Manager2: the name is editable, the id is derived from it once, and
// then the id is fixed forever.
//
// The id is the stanza directory, the step-id stem, and the prefix the
// grid index already recorded in every `qmd_path` — so the wizard
// derives it from the typed name at creation and holds it read-only
// after. This spec walks that end to end: type a name, watch the id
// derive, confirm typing into the id stops the derivation, add it, see
// the grid show the name with the id muted beside it, reopen Edit and
// find the name filled and the id disabled, then clear the name and
// watch the row fall back to the id.
//
// The fixture root's config.toml is shared by every spec in the run
// (workers: 1), so it is restored in afterEach — including on failure,
// which is what stops one broken assertion here cascading into other
// specs.
import { test, expect, type Page } from "@playwright/test";

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

const wizard = (page: Page) => page.getByRole("dialog");
// Located structurally rather than by accessible name: each field's
// `<label>` wraps its help paragraph too, so the accessible name is the
// caption plus a sentence of prose.
const field = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) > .wiz-input`);
const nameField = (page: Page) => field(page, "Name");
const idField = (page: Page) => field(page, "Id");

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

test("the id derives from the name, then stops being editable", async ({ page }) => {
  const editor = page.locator(".m2-editor");

  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  // By blurb: "Claude" alone also matches the "Claude export" tile.
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror your claude.ai conversations" })
    .click();

  // Before anything is typed: the id comes from the catalog default,
  // and the name is empty with the id as its placeholder — so a source
  // that never gets a name is still displayed by something.
  await expect(idField(page)).toHaveValue("claude");
  await expect(nameField(page)).toHaveValue("");
  await expect(nameField(page)).toHaveAttribute("placeholder", "claude");

  // Typing the name derives the id. Word order is preserved: "Personal
  // Claude" is `personal-claude`, not `claude-personal`.
  await nameField(page).fill("Personal Claude");
  await expect(idField(page)).toHaveValue("personal-claude");

  // The TOML preview is the wizard's own honesty check: the name rides
  // on the download step, and every path is built from the id.
  await wizard(page).getByText("Review the TOML this writes").click();
  const preview = wizard(page).locator(".wiz-review pre");
  await expect(preview).toContainText('name = "Personal Claude"');
  await expect(preview).toContainText('id = "personal-claude/raw"');
  await expect(preview).toContainText('id = "personal-claude/rendered_md"');
  await expect(preview).toContainText('inputs = ["personal-claude/raw"]');

  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Personal Claude.")).toBeVisible();

  // The grid shows the name, and still names the directory beside it.
  // Addressed by row id (`getRowId` is the entry's id), not by position:
  // the table lists steps and applets too.
  const row = page.locator('.ag-row[row-id="personal-claude"]');
  await expect(row).toBeVisible();
  await expect(row).toContainText("Personal Claude");
  await expect(row.locator(".m2-cell-dir")).toHaveText("personal-claude");
  await expect(editor).toHaveValue(/name = "Personal Claude"/);

  // Edit: the name is filled and free, the id is fixed.
  await row.getByRole("button", { name: "Edit" }).click();
  await expect(nameField(page)).toHaveValue("Personal Claude");
  await expect(idField(page)).toHaveValue("personal-claude");
  await expect(idField(page)).toBeDisabled();
  await expect(wizard(page)).toContainText("Use Name above for something you can change.");

  // Renaming must not touch the id — that is the whole point of the
  // split, and the thing that would silently strand the index.
  await nameField(page).fill("Claude Archive");
  await expect(idField(page)).toHaveValue("personal-claude");

  // Clearing the name removes the key, and the row falls back to the id.
  await nameField(page).fill("");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved personal-claude.")).toBeVisible();
  await expect(editor).not.toHaveValue(/name = /);
  await expect(page.locator(".ag-row", { hasText: "Personal Claude" })).toHaveCount(0);
  // Still there, displayed by its id — and with no muted second string,
  // because there is now nothing to disambiguate.
  await expect(row).toContainText("personal-claude");
  await expect(row.locator(".m2-cell-dir")).toHaveCount(0);
});

test("typing an id stops the name from driving it", async ({ page }) => {
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror your claude.ai conversations" })
    .click();

  await idField(page).fill("my-archive");
  // A derived id is a convenience, never something that overwrites a
  // choice already made.
  await nameField(page).fill("Personal Claude");
  await expect(idField(page)).toHaveValue("my-archive");

  await wizard(page).getByText("Review the TOML this writes").click();
  await expect(wizard(page).locator(".wiz-review pre")).toContainText(
    'id = "my-archive/raw"',
  );
});
