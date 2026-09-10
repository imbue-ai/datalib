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
import { test, expect, type Page } from "@playwright/test";
import { searchAndSettle } from "./grid-helpers";

const SOURCE_CELLS = '.ag-grid-scrolling-rows [col-id="source_name"]';

/// The distinct, non-empty texts in the Source column, in set order.
async function distinctSourceCells(page: Page): Promise<string[]> {
  const seen = await page.locator(SOURCE_CELLS).allInnerTexts();
  return [...new Set(seen.map((t) => t.trim()).filter(Boolean))];
}

async function openGrid(page: Page) {
  await page.goto("/");
  await page
    .locator('.ag-grid-scrolling-rows [role="row"]')
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
  // Two config writes plus four searches, any of which may land after
  // an applet restart and pay a qmd model load — see `SEARCH_SETTLE`.
  test.setTimeout(180_000);

  // --- With no config entry, the column falls back to the id -------
  await openGrid(page);
  await expect(page.locator(SOURCE_CELLS, { hasText: "slack" }).first()).toBeVisible();

  // --- `source_name:` narrows to one source ------------------------
  // Every visible cell must read `slack` — the filter is a whole-segment
  // prefix test on qmd_path, not a substring match on anything.
  await searchAndSettle(page, "source_name:slack type:all");
  await expect(page.locator(SOURCE_CELLS).first()).toBeVisible();
  expect(
    await distinctSourceCells(page),
  ).toEqual(["slack"]);

  // A stanza that exists in the fixture but isn't the one asked for
  // must be excluded, so the filter is provably doing work.
  await searchAndSettle(page, "source_name:claude-api type:all");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["claude-api"]);

  // --- A name in the config changes the column's text --------------
  await writeConfig(
    page,
    `${original.replace(/\s*$/, "")}\n
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack_api"

[[steps]]
group = "slack"
function = "ingest"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]
`,
  );

  await openGrid(page);
  await searchAndSettle(page, "source_name:slack type:all");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["Work Slack"]);

  // The filter token still carries the id, not the name: the index has
  // never heard of names, and two sources may share one.
  await searchAndSettle(page, 'source_name:"Work Slack" type:all');
  await expect(page.locator(SOURCE_CELLS)).toHaveCount(0);
});
