// The unified index grid's "Source" column, and the `source_id:`
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
import { actOnRowByUuid, searchAndSettle } from "./grid-helpers";

const SOURCE_CELLS = '.grid-box .slick-row [col-id="source_ref"]';

/// The distinct, non-empty texts in the Source column, in set order.
async function distinctSourceCells(page: Page): Promise<string[]> {
  const seen = await page.locator(SOURCE_CELLS).allInnerTexts();
  return [...new Set(seen.map((t) => t.trim()).filter(Boolean))];
}

async function openGrid(page: Page) {
  await page.goto("/");
  await page
    .locator(".grid-box .slick-row")
    .first()
    .waitFor({ timeout: 10_000 });
}

/// Replace config.toml through the Manage screen's Advanced editor,
/// which PUTs through the same validating endpoint everything else uses.
async function writeConfig(page: Page, text: string): Promise<void> {
  await page.goto("/sources2");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
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
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (original) await writeConfig(page, original);
});

test("the Source column shows the configured name, and source_id: filters by id", async ({
  page,
  request,
}) => {
  // Two config writes plus five searches, any of which may land after
  // an applet restart and pay a qmd model load — see `SEARCH_SETTLE`.
  test.setTimeout(210_000);

  // --- With no config entry, the column falls back to the id -------
  // Read off one of the source's own rows, brought into view: the grid
  // paints only the rows in view, and which those are is the sort's
  // business, not this test's.
  const slackRow = (await (
    await request.get("/applet/unified_index/search?q=source_id:slack&limit=1")
  ).json()) as { rows: { uuid: string }[] };
  expect(slackRow.rows.length, "the fixture has slack rows").toBe(1);
  await openGrid(page);
  const cellText = await actOnRowByUuid(
    page,
    slackRow.rows[0].uuid,
    (row) => row.locator('[col-id="source_ref"]').innerText({ timeout: 3_000 }),
    "source_ref",
  );
  expect(cellText.trim()).toBe("slack");

  // --- `source_id:` narrows to one source --------------------------
  // Every visible cell must read `slack` — the filter is a whole-segment
  // prefix test on qmd_path, not a substring match on anything.
  await searchAndSettle(page, "source_id:slack");
  await expect(page.locator(SOURCE_CELLS).first()).toBeVisible();
  expect(
    await distinctSourceCells(page),
  ).toEqual(["slack"]);

  // `source_name:` is the spelling this filter had before a source had
  // a name to collide with, so it is in saved queries and in people's
  // fingers. It has to keep landing on the same rows.
  await searchAndSettle(page, "source_name:slack");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["slack"]);

  // A stanza that exists in the fixture but isn't the one asked for
  // must be excluded, so the filter is provably doing work.
  await searchAndSettle(page, "source_id:claude-api");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["claude-api"]);

  // Datalib's own rows — every source's storage report — are filed
  // under `datalib` rather than the source they measure, which is why
  // neither search above turned one up. They have their own bucket,
  // and the column spells it out.
  await searchAndSettle(page, "source_id:datalib");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["Datalib"]);

  // --- A name in the config changes the column's text --------------
  await writeConfig(
    page,
    `${original.replace(/\s*$/, "")}\n
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack"

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
  await searchAndSettle(page, "source_id:slack");
  expect(
    await distinctSourceCells(page),
  ).toEqual(["Work Slack"]);

  // The filter token still carries the id, not the name: the index has
  // never heard of names, and two sources may share one.
  await searchAndSettle(page, 'source_id:"Work Slack"');
  await expect(page.locator(SOURCE_CELLS)).toHaveCount(0);
});
