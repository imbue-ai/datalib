// Manager2: one row per step, and the two-step flow that creates them.
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
/// The step-role mark. It rides after the name — there is no Step
/// column any more — and `aria-label` is the only place the word
/// survives, which is also what a person gets by hovering it.
const stepMark = (page: Page, id: string) =>
  row(page, id).locator('[col-id="name"] .m2-name-step [role="img"]');

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

  // ...and the card around it is a real block, not a sliver.
  const card = wizard(page).locator("label.wiz-check");
  const box = await card.boundingBox();
  expect(box, "the also-render card should be laid out").not.toBeNull();
  expect(box!.width, "the also-render card collapsed to its checkbox").toBeGreaterThan(200);
  await wizard(page).getByText("Review the TOML this writes").click();
  const preview = wizard(page).locator(".wiz-review pre");
  // The name lands on the group; the two steps are written as
  // `group` + `function` and carry none.
  await expect(preview).toContainText('id = "personal-claude"');
  await expect(preview).toContainText('name = "Personal Claude"');
  await expect(preview).toContainText('type = "claude_api"');
  await expect(preview).toContainText('function = "ingest"');
  await expect(preview).toContainText('function = "render_markdown"');
  await expect(preview).toContainText('inputs = ["personal-claude/ingest"]');

  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText(/Added Personal Claude, with a step to render it\./)).toBeVisible();

  // Two rows, not one. Addressed by row id (`getRowId` is the step id).
  //
  // The phase is a glyph suffixed onto the name, so it is asserted
  // through the accessible name rather than cell text.
  await expect(row(page, "personal-claude/ingest")).toContainText("Personal Claude");
  await expect(stepMark(page, "personal-claude/ingest")).toHaveAttribute("aria-label", "Ingest");
  await expect(row(page, "personal-claude/render_markdown")).toContainText(
    "Personal Claude (render markdown)",
  );
  await expect(stepMark(page, "personal-claude/render_markdown")).toHaveAttribute(
    "aria-label",
    "Render",
  );

  // The render step is written once, as its own entry. The fixture
  // root's config declares no index steps (its grid db is pre-baked),
  // so there is nothing here for `wireIntoFanIns` to add it to — that
  // wiring is covered in source_steps.test.ts against a config that has
  // fan-ins.
  const text = await editor.inputValue();
  expect(text.match(/group = "personal-claude"\nfunction = "render_markdown"/g)).toHaveLength(1);

  // Edit the fetch step: name free, id fixed, and renaming leaves the
  // id alone — the property that keeps the index's paths honest.
  await row(page, "personal-claude/ingest").getByRole("button", { name: "Edit" }).click();
  await expect(nameField(page)).toHaveValue("Personal Claude");
  await expect(idField(page)).toHaveCount(0);
  await expect(wizard(page).locator(".wiz-fixed-id")).toContainText("personal-claude/ingest");
  await nameField(page).fill("Claude Archive");
  await expect(wizard(page).locator(".wiz-fixed-id")).toContainText("personal-claude/ingest");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved Claude Archive.")).toBeVisible();

  // The name belongs to the group, so renaming the source renames both
  // rows: the render step's label is the same name said again.
  await expect(row(page, "personal-claude/render_markdown")).toContainText(
    "Claude Archive (render markdown)",
  );
  await expect(editor).toHaveValue(/name = "Claude Archive"/);
  await expect(editor).not.toHaveValue(/Personal Claude/);
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

  await expect(row(page, "fetch-only/ingest")).toBeVisible();
  await expect(page.locator('.ag-row[row-id="fetch-only/render_markdown"]')).toHaveCount(0);
  await expect(editor).not.toHaveValue(/fetch-only\/render_markdown/);

  // The row action adds it later, minting the sibling id through the
  // same path the checkbox would have.
  await row(page, "fetch-only/ingest").getByRole("button", { name: "Render to markdown" }).click();
  await expect(idField(page)).toHaveCount(0);
  await expect(wizard(page)).toContainText("fetch-only/render_markdown");
  // No name box: the render step's label comes from the group's name.
  await expect(nameField(page)).toHaveCount(0);
  await wizard(page).getByRole("button", { name: "Add render step" }).click();

  await expect(row(page, "fetch-only/render_markdown")).toBeVisible();
  await expect(editor).toHaveValue(/inputs = \["fetch-only\/ingest"\]/);

  // ...and now the action is spent: there is already a render step.
  const again = row(page, "fetch-only/ingest").getByRole("button", { name: "Render to markdown" });
  await expect(again).toBeDisabled();
  await expect(again).toHaveAttribute("title", /already has a render step/);
});

test("a provider whose render step has options writes the sibling id, not the stem", async ({
  page,
}) => {
  // The path Claude never takes, and the one that was broken.
  //
  // That config is not merely untidy — it does not run. `datalib-dag`
  // rejects it with "a step writes only the tree its id names", which
  // is a red Status on a source that downloaded perfectly well, and
  // `phaseOf` reads the stem as `other`, so the step is never wired
  // into the fan-ins either. Two failures, one cause.
  //
  // A provider whose *render* step has options gets a second dialog
  // instead of the checkbox: `onWizardSubmit` closes the wizard and
  // reopens it for the render step. Both happen in one synchronous
  // stretch — `window.confirm` blocks the event loop — so `wizardOpen`
  // went false and true again inside a single tick, Vue never flushed
  // the false, and the component was *reused* rather than remounted.
  // Every ref the wizard sets up in `setup()` therefore kept its
  // create-mode value, and the id is the one that mattered: the render
  // step was written as `signal-work` (the stem) instead of
  // `signal-work/render_markdown`.
  const editor = page.locator(".m2-editor");
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Decrypt and mirror an Android Signal backup" })
    .click();
  await nameField(page).fill("Signal Work");
  await wizard(page).locator("input.wiz-path").fill("/tmp/SignalBackups");
  await expect(idField(page)).toHaveValue("signal-work");

  // No checkbox: this provider's render step has a `period` option, so
  // the offer is a confirm and then a second dialog.
  await expect(wizard(page).locator("label.wiz-check")).toHaveCount(0);
  page.once("dialog", (d) => {
    expect(d.message()).toContain("Also render it to markdown?");
    void d.accept();
  });
  await wizard(page).getByRole("button", { name: "Add source" }).click();

  // The second dialog, freshly mounted: the sibling id, not inherited
  // from the dialog that just closed. No name box — a render step's
  // label is derived from its group's name.
  await expect(nameField(page)).toHaveCount(0);
  await expect(wizard(page)).toContainText("signal-work/render_markdown");
  await wizard(page).getByRole("button", { name: "Add render step" }).click();

  await expect(row(page, "signal-work/render_markdown")).toBeVisible();
  await expect(stepMark(page, "signal-work/render_markdown")).toHaveAttribute("aria-label", "Render");

  const text = await editor.inputValue();
  expect(text).toContain('group = "signal-work"\nfunction = "render_markdown"');
  expect(text).toContain('inputs = ["signal-work/ingest"]');
  // One group, written once, by the first dialog — the second wrote a
  // step under it and nothing else.
  expect(text.match(/id = "signal-work"/g)).toHaveLength(1);
});

test("deleting a fetch step takes its render step with it", async ({ page }) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Doomed");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(row(page, "doomed/render_markdown")).toBeVisible();

  // A render step whose input is gone is a config datalib refuses to
  // load, so delete offers both or neither.
  page.once("dialog", (d) => {
    expect(d.message()).toContain("Doomed (render markdown)");
    void d.accept();
  });
  await row(page, "doomed/ingest").getByRole("button", { name: "Remove from config" }).click();
  await expect(page.getByText("Removed Doomed.")).toBeVisible();

  await expect(page.locator('.ag-row[row-id="doomed/ingest"]')).toHaveCount(0);
  await expect(page.locator('.ag-row[row-id="doomed/render_markdown"]')).toHaveCount(0);
  // Including the fan-in references, or the config would not load.
  await expect(editor).not.toHaveValue(/doomed/);
});
