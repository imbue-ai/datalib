// The unified index grid's "Source" column, and the `source_name:`
// filter behind it.
//
// The column answers a question the "Provider" column cannot: which
// *configured source* did this row come from. Two Slack workspaces are
// one provider and two sources. The value on the row is the source's id
// (derived server-side from `qmd_path`); the text in the cell is the
// `name` that source's steps declare in config.toml, joined
// client-side. That join is the thing worth an end-to-end test — it
// crosses the backend, the config file and the grid, and it is the
// reason renaming a source never needs a re-index.
//
// The fixture data root's config.toml declares no sources, so the
// column shows raw ids until this spec adds one. It restores the file
// in afterEach: the root is shared by every spec in the run
// (workers: 1, fullyParallel: false).
import { test, expect, type Page } from "@playwright/test";

const SOURCE_CELLS = '.ag-center-cols-container [col-id="source_name"]';

async function openGrid(page: Page) {
  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });
}

/// Replace config.toml through the Manage screen's Advanced editor,
/// which PUTs through the same validating endpoint everything else uses.
async function writeConfig(page: Page, text: string): Promise<void> {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
}

// Captured before the test edits it and put back afterwards even when
// the test fails. Restoring inline at the end would mean one failing
// assertion here leaves a source in the config and takes every later
// spec down with it.
let original = "";

test.beforeEach(async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (original) await writeConfig(page, original);
});

test("the Source column shows the configured name, and source_name: filters by id", async ({
  page,
}) => {
  // --- With no config entry, the column falls back to the id -------
  await openGrid(page);
  await expect(page.locator(SOURCE_CELLS, { hasText: "slack" }).first()).toBeVisible();

  // --- `source_name:` narrows to one source ------------------------
  // Every visible cell must read `slack` — the filter is a whole-segment
  // prefix test on qmd_path, not a substring match on anything.
  await page.getByTestId("search-input").fill("source_name:slack type:all");
  await expect(page.locator(SOURCE_CELLS).first()).toBeVisible();
  await expect
    .poll(async () => {
      const seen = await page.locator(SOURCE_CELLS).allInnerTexts();
      return [...new Set(seen.map((t) => t.trim()).filter(Boolean))];
    })
    .toEqual(["slack"]);

  // A stanza that exists in the fixture but isn't the one asked for
  // must be excluded, so the filter is provably doing work.
  await page.getByTestId("search-input").fill("source_name:anthropic-api type:all");
  await expect
    .poll(async () => {
      const seen = await page.locator(SOURCE_CELLS).allInnerTexts();
      return [...new Set(seen.map((t) => t.trim()).filter(Boolean))];
    })
    .toEqual(["anthropic-api"]);

  // --- A name in the config changes the column's text --------------
  await writeConfig(
    page,
    `${original.replace(/\s*$/, "")}\n
[[steps]]
id = "slack.download"
name = "Work Slack"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]

[[steps]]
id = "slack.render"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
outputs = ["slack/rendered_md"]
`,
  );

  await openGrid(page);
  await page.getByTestId("search-input").fill("source_name:slack type:all");
  await expect
    .poll(async () => {
      const seen = await page.locator(SOURCE_CELLS).allInnerTexts();
      return [...new Set(seen.map((t) => t.trim()).filter(Boolean))];
    })
    .toEqual(["Work Slack"]);

  // The filter token still carries the id, not the name: the index has
  // never heard of names, and two sources may share one.
  await page.getByTestId("search-input").fill("source_name:Work Slack type:all");
  await expect(page.locator(SOURCE_CELLS)).toHaveCount(0);
});
