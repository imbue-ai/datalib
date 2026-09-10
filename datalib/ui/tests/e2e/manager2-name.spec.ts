// Manager2: one row per group with its steps under it, and the one
// dialog that creates and edits them.
import { test, expect, type Locator, type Page } from "@playwright/test";
import { expandGroup, groupRow, pipelineRow as row } from "./grid-helpers";

/// Click a row action and wait for what it does, retrying the pair.
///
/// A save is followed by the server's `config_changed`, which reloads
/// the config and force-repaints the actions column. A click whose
/// mousedown lands on the old button and whose mouseup lands on its
/// replacement fires no click at all, so a bare `.click()` right after
/// a save is a race — the same one `actOnRowByUuid` retries around in
/// grid-helpers. `effect` already visible means an earlier attempt
/// landed, so nothing is clicked twice.
async function clickUntil(button: Locator, effect: Locator): Promise<void> {
  await expect(async () => {
    if (await effect.isVisible()) return;
    await button.click({ timeout: 2_000 });
    await expect(effect).toBeVisible({ timeout: 2_000 });
  }, `${await button.getAttribute("title").catch(() => "the action")} never took`).toPass({
    timeout: 15_000,
    intervals: [250, 500, 1_000],
  });
}

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

test("one dialog writes a group and two steps: one row, with two under it", async ({
  page,
}) => {
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

  // The render step is part of the source, not an offer: the form says
  // what it writes under a Rendering heading, and the preview shows
  // both steps so the heading demonstrates its consequence rather than
  // asserting it. Claude's render step has no settings, so the heading
  // is followed by a sentence and no fields.
  await expect(wizard(page).locator(".wiz-section-head")).toHaveText("Rendering");
  await expect(wizard(page).locator(".wiz-section")).toContainText("personal-claude/render_markdown");
  await expect(wizard(page).locator(".wiz-section")).toContainText("no settings of its own");
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
  await expect(page.getByText("Added Personal Claude.")).toBeVisible();

  // One row for the source: the group's name, with the id muted
  // beside it. The steps are under it, and folded until asked for —
  // which is the whole point of the row.
  const group = groupRow(page, "personal-claude");
  await expect(group).toContainText("Personal Claude");
  await expect(group.locator(".m2-cell-dir")).toHaveText("personal-claude");
  await expect(row(page, "personal-claude/ingest")).toHaveCount(0);

  // Opened, the two steps are labelled by what they do; the group owns
  // the name. The ingest step reads "Download" because its `sync` table
  // reaches claude.ai. The phase is a glyph suffixed onto the label, so
  // it is asserted through the accessible name rather than cell text.
  await expandGroup(page, "personal-claude");
  await expect(row(page, "personal-claude/ingest")).toContainText("Download");
  await expect(row(page, "personal-claude/ingest")).toContainText("personal-claude/ingest");
  await expect(stepMark(page, "personal-claude/ingest")).toHaveAttribute("aria-label", "Ingest");
  await expect(row(page, "personal-claude/render_markdown")).toContainText("Render markdown");
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

  // Edit from the group's row: the same one dialog, over the source.
  // Name free, id fixed, and renaming leaves the id alone — the
  // property that keeps the index's paths honest.
  await clickUntil(group.getByRole("button", { name: "Edit settings" }), wizard(page));
  await expect(nameField(page)).toHaveValue("Personal Claude");
  await expect(idField(page)).toHaveCount(0);
  await expect(wizard(page).locator(".wiz-fixed-id")).toContainText("personal-claude/");
  await nameField(page).fill("Claude Archive");
  await expect(wizard(page).locator(".wiz-fixed-id")).toContainText("personal-claude/");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved Claude Archive.")).toBeVisible();

  // The name belongs to the group, so the group row renames and the
  // steps under it — labelled by what they do — do not. Saving rewrote
  // both steps and left exactly one of each.
  await expect(groupRow(page, "personal-claude")).toContainText("Claude Archive");
  await expect(row(page, "personal-claude/render_markdown")).toContainText("Render markdown");
  await expect(editor).toHaveValue(/name = "Claude Archive"/);
  await expect(editor).not.toHaveValue(/Personal Claude/);
  const saved = await editor.inputValue();
  expect(saved.match(/group = "personal-claude"\nfunction = "ingest"/g)).toHaveLength(1);
  expect(saved.match(/group = "personal-claude"\nfunction = "render_markdown"/g)).toHaveLength(1);
});

test("a step's Edit opens its source, and a hand-removed render step comes back on save", async ({
  page,
}) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Fetch Only");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Fetch Only.")).toBeVisible();

  // Take the render step out by hand, the way a config edited in an
  // editor might lack one.
  const text = await editor.inputValue();
  const without = text.replace(
    /\n\[\[steps\]\]\ngroup = "fetch-only"\nfunction = "render_markdown"\ninputs = \["fetch-only\/ingest"\]\n/,
    "\n",
  );
  expect(without).not.toBe(text);
  await page.getByText("Advanced — edit config.toml directly").click();
  await editor.fill(without);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();

  await expandGroup(page, "fetch-only");
  await expect(row(page, "fetch-only/ingest")).toBeVisible();
  await expect(page.locator('.ag-row[row-id="fetch-only/render_markdown"]')).toHaveCount(0);

  // A step under a group edits its source: the step row's button opens
  // the same dialog the group row's does, name box and all.
  await clickUntil(
    row(page, "fetch-only/ingest").getByRole("button", { name: "Edit settings" }),
    wizard(page),
  );
  await expect(nameField(page)).toHaveValue("Fetch Only");
  // The dialog says what saving will do beyond changing a value.
  await expect(wizard(page)).toContainText("This source is missing");
  await expect(wizard(page)).toContainText("fetch-only/render_markdown");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved Fetch Only.")).toBeVisible();

  await expect(row(page, "fetch-only/render_markdown")).toBeVisible();
  await expect(editor).toHaveValue(/inputs = \["fetch-only\/ingest"\]/);
  const after = await editor.inputValue();
  expect(after.match(/group = "fetch-only"\nfunction = "ingest"/g)).toHaveLength(1);
});

test("a provider with render options writes them on the render step, from the one form", async ({
  page,
}) => {
  // Signal's render step has a `period` option. Before the one-dialog
  // wizard it came as a second dialog, and the remount between the two
  // once wrote the render step under the group's id instead of its own
  // — a config `datalib-dag` refuses with "a step writes only the tree
  // its id names". One dialog has no second mount to get wrong; the
  // assertion on the composed id stays because that is the config bug
  // it would catch.
  const editor = page.locator(".m2-editor");
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Decrypt and mirror an Android Signal backup" })
    .click();
  await nameField(page).fill("Signal Work");
  await wizard(page).locator("input.wiz-path").fill("/tmp/SignalBackups");
  await expect(idField(page)).toHaveValue("signal-work");

  // The render option sits under the Rendering heading, in this form.
  const section = wizard(page).locator(".wiz-section");
  await expect(section).toContainText("signal-work/render_markdown");
  await expect(field(page, "Document span")).toHaveValue("month");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Signal Work.")).toBeVisible();

  await expandGroup(page, "signal-work");
  await expect(row(page, "signal-work/render_markdown")).toBeVisible();
  await expect(stepMark(page, "signal-work/render_markdown")).toHaveAttribute("aria-label", "Render");

  const text = await editor.inputValue();
  expect(text).toContain('group = "signal-work"\nfunction = "render_markdown"');
  expect(text).toContain('inputs = ["signal-work/ingest"]');
  expect(text).toContain('period = "month"');
  // One group, written once.
  expect(text.match(/id = "signal-work"/g)).toHaveLength(1);
});

test("a hand-written render step under a download-only type is called out, then removed", async ({
  page,
}) => {
  // Lightroom renders nothing, so the form writes one step. A render
  // step someone wrote by hand under it cannot be kept — the provider
  // has no render side — and the dialog says so before Save, the way
  // it does for a missing step. (Unwiring it from the fan-ins is
  // covered by the unit tests; this root's config declares none.)
  const editor = page.locator(".m2-editor");
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror a Lightroom Classic catalog" })
    .click();
  await nameField(page).fill("Photos");
  await wizard(page).locator("input.wiz-path").fill("/tmp/cat.lrcat");
  await expect(wizard(page).locator(".wiz-section-head")).toHaveCount(0);
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Photos.")).toBeVisible();

  const text = await editor.inputValue();
  expect(text).toContain('group = "photos"\nfunction = "ingest"');
  expect(text).not.toContain('group = "photos"\nfunction = "render_markdown"');
  await page.getByText("Advanced — edit config.toml directly").click();
  await editor.fill(
    `${text.trimEnd()}\n\n[[steps]]\ngroup = "photos"\nfunction = "render_markdown"\ninputs = ["photos/ingest"]\n`,
  );
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
  await expandGroup(page, "photos");
  await expect(row(page, "photos/render_markdown")).toBeVisible();

  await clickUntil(
    groupRow(page, "photos").getByRole("button", { name: "Edit settings" }),
    wizard(page),
  );
  await expect(wizard(page)).toContainText("Lightroom renders nothing. Saving removes it");
  await wizard(page).getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Saved Photos.")).toBeVisible();
  await expect(page.locator('.ag-row[row-id="photos/render_markdown"]')).toHaveCount(0);
  await expect(editor).not.toHaveValue(/group = "photos"\nfunction = "render_markdown"/);
  await expect(editor).toHaveValue(/group = "photos"\nfunction = "ingest"/);
});

test("deleting a fetch step takes its render step with it", async ({ page }) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Doomed");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Doomed.")).toBeVisible();
  await expandGroup(page, "doomed");
  await expect(row(page, "doomed/render_markdown")).toBeVisible();

  // A render step whose input is gone is a config datalib refuses to
  // load, so delete offers both or neither. `on`, not `once`: the click
  // is retried, and a retry that reaches the confirm needs answering.
  page.on("dialog", (d) => {
    expect(d.message()).toContain("Doomed (render markdown)");
    void d.accept();
  });
  await clickUntil(
    row(page, "doomed/ingest").getByRole("button", { name: "Remove from config" }),
    page.getByText("Removed Doomed."),
  );

  // The group went with its last step, so its row is gone too.
  await expect(page.locator('.ag-row[row-id="doomed/ingest"]')).toHaveCount(0);
  await expect(page.locator('.ag-row[row-id="doomed/render_markdown"]')).toHaveCount(0);
  await expect(groupRow(page, "doomed")).toHaveCount(0);
  // Including the fan-in references, or the config would not load.
  await expect(editor).not.toHaveValue(/doomed/);
});

test("deleting the group takes every step under it", async ({ page }) => {
  const editor = page.locator(".m2-editor");
  await pickClaude(page);
  await nameField(page).fill("Whole Group");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(groupRow(page, "whole-group")).toBeVisible();
  await expect(editor).toHaveValue(/group = "whole-group"\nfunction = "render_markdown"/);

  // The confirm says what goes: the group and the two steps under it.
  page.on("dialog", (d) => {
    expect(d.message()).toContain("Whole Group");
    expect(d.message()).toContain("2 steps");
    void d.accept();
  });
  await clickUntil(
    groupRow(page, "whole-group").getByRole("button", {
      name: "Remove from config, with everything under it",
    }),
    page.getByText("Removed Whole Group."),
  );

  await expect(groupRow(page, "whole-group")).toHaveCount(0);
  await expect(page.locator('.ag-row[row-id^="whole-group/"]')).toHaveCount(0);
  // The `[[groups]]` entry, both `[[steps]]`, and any fan-in reference:
  // nothing of it is left in the file.
  await expect(editor).not.toHaveValue(/whole-group/);
});
