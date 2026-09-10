// Browse, from a row on the Manage screen to the rows it stands for.
//
// The chain this covers is the point of the button, and no unit test can
// reach it: a group row knows its id and type, the button writes a card
// source naming both, the router carries that source in the URL, the
// card compiles it, and the grid comes back holding one source's rows
// with that type's columns. Every link is in a different file.

import { test, expect, type Page } from "@playwright/test";

const ROWS = '.ag-grid-scrolling-rows [role="row"]';
const SEARCH = '[data-testid="search-input"]';

/// The fixture root declares the `unified_index` group (as a real root
/// does) but no sources — the per-source rendered trees arrive as tars,
/// with nothing in the config describing them. So the sources this spec
/// browses are declared here first, the way `grid-source-name.spec.ts`
/// does it.
///
/// `media` is the negative case and is the real shape: it is one of the
/// three download-only providers, so a config for it genuinely has no
/// `render_markdown` step.
const GROUPS = `
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

[[groups]]
id = "github"
type = "github"

[[steps]]
group = "github"
function = "ingest"

[[steps]]
group = "github"
function = "render_markdown"
inputs = ["github/ingest"]

[[groups]]
id = "media"
type = "media"

[[steps]]
group = "media"
function = "ingest"
`;

async function openManage(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  await page.locator(ROWS).first().waitFor({ timeout: 10_000 });
}

async function writeConfig(page: Page, text: string): Promise<void> {
  await openManage(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
}

/// The Browse button on a group's row. Group rows carry `group:<id>` as
/// their AG Grid row id (see `groupRowKey`), which beats matching on the
/// Name cell — that cell also renders the directory name beside the
/// label.
function browseButton(page: Page, groupId: string) {
  return page
    .locator(`${ROWS}[row-id="group:${groupId}"]`)
    .locator('button[aria-label^="Browse"]');
}

/// Click Browse and wait until the grid card is actually up.
///
/// Both screens are AG Grids, so "a row exists" is true on the Manage
/// screen before the click has gone anywhere — waiting on that alone
/// reads the old page and fails somewhere far from the cause. The search
/// box belongs to the card and to nothing else, so it is the honest
/// signal that the navigation landed.
async function browse(page: Page, groupId: string, expectQuery: string) {
  await browseButton(page, groupId).click();
  await expect(page.locator(SEARCH)).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(SEARCH)).toHaveValue(expectQuery);
  await page.locator(ROWS).first().waitFor({ timeout: 30_000 });
}

// Captured before this spec edits it and put back afterwards even on
// failure — leaving sources in the config would take later specs down.
let original = "";

test.beforeEach(async ({ page }) => {
  await openManage(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  original = await page.locator(".m2-editor").inputValue();
  await writeConfig(page, `${original.replace(/\s*$/, "")}\n${GROUPS}`);
});

test.afterEach(async ({ page }) => {
  if (original) await writeConfig(page, original);
});

test("a source's row opens that source, with its type's columns", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  await browse(page, "slack", "source_name:slack");

  // The card stack IS the URL, which is what makes a browse
  // bookmarkable and shareable rather than a transient view.
  await expect(page).toHaveURL(/source_name%3Aslack/);

  // Every row came from this source. The Source column is hidden here —
  // one value, so the adaptive rule drops it — which is why this reads
  // the Type column instead.
  const kinds = await page
    .locator('.ag-grid-scrolling-rows [col-id="kind"]')
    .allInnerTexts();
  expect(kinds.length).toBeGreaterThan(0);
  for (const k of kinds) {
    expect(k.trim()).toMatch(/Slack|Source Size|Table/);
  }

  // Slack's preset: a channel and an author, and no Project — Slack has
  // no such thing, and a column of empty cells is what a preset exists
  // to prevent.
  await expect(page.locator('.ag-header-cell[col-id="channel"]')).toBeVisible();
  await expect(page.locator('.ag-header-cell[col-id="author"]')).toBeVisible();
  await expect(page.locator('.ag-header-cell[col-id="project"]')).toHaveCount(0);
});

test("a different type gets a different column set", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  await browse(page, "github", "source_name:github");

  // GitHub's `project` is the repository it belongs to, and it has no
  // channel. The opposite pair to Slack's, from the same fixture.
  await expect(page.locator('.ag-header-cell[col-id="project"]')).toBeVisible();
  await expect(page.locator('.ag-header-cell[col-id="channel"]')).toHaveCount(0);
});

test("the index group browses every source", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  // No filter: the index group's browse is every source at once.
  await browse(page, "unified_index", "");

  // The column that separates sources is the one that earns its place
  // here — the opposite of a per-source browse.
  const sources = await page
    .locator('.ag-grid-scrolling-rows [col-id="source_name"]')
    .allInnerTexts();
  const distinct = new Set(sources.map((s) => s.trim()).filter(Boolean));
  expect(distinct.size).toBeGreaterThan(1);
});

/// A source that renders nothing has no rows at all — not even the
/// storage rows, which render is what emits. The button says so rather
/// than opening an empty grid onto a source that looks broken.
test("a download-only source cannot be browsed", async ({ page }) => {
  await openManage(page);
  const media = browseButton(page, "media");
  await expect(media).toBeDisabled();
  await expect(media).toHaveAttribute("title", /no render step/);
});
