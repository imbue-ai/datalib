// Manager2: one row per step, and the two-step flow that creates them.
//
// A fetch step and the render step that reads it are separate rows,
// separately editable and separately runnable. The wizard writes the
// fetch step; the render step comes either from a checkbox (when the
// provider has no render options, which is all but one of them) or from
// a row action later.
//
// The id is the tree a step writes, so it is fixed after creation. The
// name is what a person types, derived once into the id and freely
// changed after.
//
// The fixture root's config.toml is shared by every spec in the run
// (workers: 1), so it is restored in afterEach — including on failure,
// which is what stops one broken assertion here cascading.
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
const alsoRender = (page: Page) => wizard(page).locator(".wiz-check input");
const row = (page: Page, id: string) => page.locator(`.ag-row[row-id="${id}"]`);

async function pickClaude(page: Page) {
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  // By blurb: "Claude" alone also matches the "Claude export" tile.
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror your claude.ai conversations" })
    .click();
}

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

test("one dialog writes two steps, and they are two rows", async ({ page }) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);

  // Before anything is typed: the id comes from the catalog default,
  // and the name is empty with the id as its placeholder.
  await expect(idField(page)).toHaveValue("claude");
  await expect(nameField(page)).toHaveValue("");

  // Typing the name derives the id. Word order is preserved: "Personal
  // Claude" is `personal-claude`, not `claude-personal`.
  await nameField(page).fill("Personal Claude");
  await expect(idField(page)).toHaveValue("personal-claude");

  // Claude has no render options, so the render step is a checkbox
  // rather than a second dialog — and the preview shows both steps, so
  // the checkbox demonstrates its consequence instead of asserting it.
  await expect(alsoRender(page)).toBeChecked();
  await wizard(page).getByText("Review the TOML this writes").click();
  const preview = wizard(page).locator(".wiz-review pre");
  await expect(preview).toContainText('id = "personal-claude/raw"');
  await expect(preview).toContainText('name = "Personal Claude"');
  await expect(preview).toContainText('id = "personal-claude/rendered_md"');
  await expect(preview).toContainText('inputs = ["personal-claude/raw"]');

  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText(/Added Personal Claude, with a step to render it\./)).toBeVisible();

  // Two rows, not one. Addressed by row id (`getRowId` is the step id).
  //
  // The Step column is a glyph, so the phase is asserted through the
  // accessible name rather than cell text — which is the same string a
  // person gets by hovering it, and the only place the word survives.
  await expect(row(page, "personal-claude/raw")).toContainText("Personal Claude");
  await expect(
    row(page, "personal-claude/raw").locator('[col-id="kindLabel"] [role="img"]'),
  ).toHaveAttribute("aria-label", "Fetch");
  await expect(row(page, "personal-claude/rendered_md")).toContainText(
    "Personal Claude (render markdown)",
  );
  await expect(
    row(page, "personal-claude/rendered_md").locator('[col-id="kindLabel"] [role="img"]'),
  ).toHaveAttribute("aria-label", "Render");

  // The render step is written once, as its own entry. The fixture
  // root's config declares no index steps (its grid db is pre-baked),
  // so there is nothing here for `wireIntoFanIns` to add it to — that
  // wiring is covered in source_steps.test.ts against a config that has
  // fan-ins.
  const text = await editor.inputValue();
  expect(text.match(/"personal-claude\/rendered_md"/g)).toHaveLength(1);

  // Edit the fetch step: name free, id fixed, and renaming leaves the
  // id alone — the property that keeps the index's paths honest.
  await row(page, "personal-claude/raw").getByRole("button", { name: "Edit" }).click();
  await expect(nameField(page)).toHaveValue("Personal Claude");
  await expect(idField(page)).toHaveValue("personal-claude/raw");
  await expect(idField(page)).toBeDisabled();
  await nameField(page).fill("Claude Archive");
  await expect(idField(page)).toHaveValue("personal-claude/raw");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved Claude Archive.")).toBeVisible();

  // The render step keeps its own name: they are separate steps, and
  // renaming one is not renaming the other.
  await expect(row(page, "personal-claude/rendered_md")).toContainText(
    "Personal Claude (render markdown)",
  );
});

test("declining the checkbox writes one step, and the row action adds the other", async ({
  page,
}) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Fetch Only");
  await alsoRender(page).uncheck();
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Fetch Only.")).toBeVisible();

  await expect(row(page, "fetch-only/raw")).toBeVisible();
  await expect(page.locator('.ag-row[row-id="fetch-only/rendered_md"]')).toHaveCount(0);
  await expect(editor).not.toHaveValue(/fetch-only\/rendered_md/);

  // The row action adds it later, minting the sibling id through the
  // same path the checkbox would have.
  await row(page, "fetch-only/raw").getByRole("button", { name: "Render to markdown" }).click();
  await expect(idField(page)).toHaveValue("fetch-only/rendered_md");
  await expect(idField(page)).toBeDisabled();
  await expect(nameField(page)).toHaveValue("Fetch Only (render markdown)");
  await wizard(page).getByRole("button", { name: "Add render step" }).click();

  await expect(row(page, "fetch-only/rendered_md")).toBeVisible();
  await expect(editor).toHaveValue(/inputs = \["fetch-only\/raw"\]/);

  // ...and now the action is spent: there is already a render step.
  const again = row(page, "fetch-only/raw").getByRole("button", { name: "Render to markdown" });
  await expect(again).toBeDisabled();
  await expect(again).toHaveAttribute("title", /already has a render step/);
});

test("deleting a fetch step takes its render step with it", async ({ page }) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Doomed");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(row(page, "doomed/rendered_md")).toBeVisible();

  // A render step whose input is gone is a config datalib refuses to
  // load, so delete offers both or neither.
  page.once("dialog", (d) => {
    expect(d.message()).toContain("Doomed (render markdown)");
    void d.accept();
  });
  await row(page, "doomed/raw").getByRole("button", { name: "Remove from config" }).click();
  await expect(page.getByText("Removed Doomed.")).toBeVisible();

  await expect(page.locator('.ag-row[row-id="doomed/raw"]')).toHaveCount(0);
  await expect(page.locator('.ag-row[row-id="doomed/rendered_md"]')).toHaveCount(0);
  // Including the fan-in references, or the config would not load.
  await expect(editor).not.toHaveValue(/doomed/);
});
