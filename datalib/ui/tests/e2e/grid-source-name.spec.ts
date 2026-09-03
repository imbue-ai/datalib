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
  await searchAndSettle(page, "source_name:anthropic-api type:all");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["anthropic-api"]);

  // --- A name in the config changes the column's text --------------
  await writeConfig(
    page,
    `${original.replace(/\s*$/, "")}\n
[[steps]]
id = "slack/raw"
name = "Work Slack"
command = "datalib-step download slack_api"

[[steps]]
id = "slack/rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
`,
  );

  await openGrid(page);
  await searchAndSettle(page, "source_name:slack type:all");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["Work Slack"]);

  // The filter token still carries the id, not the name: the index has
  // never heard of names, and two sources may share one.
  //
  // The name is quoted because it contains a space. Unquoted, the
  // parser takes `source_name:Work` and leaves `Slack` as a bare term
  // (`tokenize` splits on unquoted whitespace, `split_field` only
  // claims up to the first unquoted `:`) — and a bare term is free
  // text, which routes through qmd. This spec sorts long before
  // `search-qmd-routing`, so that stray word made it the session's
  // *first* qmd call and it silently paid the model load the `warmup`
  // project now owns. It read as a flake — fine on a warm idle machine,
  // dead under `--runs_per_test=N` where every sandbox pays at once —
  // and it weakened the assertion too, since zero rows could have come
  // from the free-text clause rather than from `source_name:`. Quoted,
  // this is one structured filter and no qmd at all.
  //
  // A flat `toHaveCount(0)` is safe here only because `searchAndSettle`
  // has already established that the grid is painting *this* query —
  // otherwise it would race the repaint and pass or fail on how fast
  // the search came back.
  await searchAndSettle(page, 'source_name:"Work Slack" type:all');
  await expect(page.locator(SOURCE_CELLS)).toHaveCount(0);
});
