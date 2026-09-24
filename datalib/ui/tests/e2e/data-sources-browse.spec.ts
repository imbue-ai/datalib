// Browse, from a row on the Manage screen to the rows it stands for.
//
// The chain this covers is the point of the button, and no unit test can
// reach it: a group row knows its id and type, the button writes a card
// source naming both, the router carries that source in the URL, the
// card compiles it, and the grid comes back holding one source's rows
// with that type's columns. Every link is in a different file.

import { test, expect, type Page } from "@playwright/test";
import {
  searchAndSettle,
  SEARCH_ROWS,
  TABLE_ROWS,
  searchHeader,
  type GridApi,
  MANAGE_WITH_CONFIG,
  expandGroup,
  pipelineRow,
} from "./grid-helpers";

const ROWS = TABLE_ROWS;
const SEARCH = '[data-testid="search-input"]';

/// The fixture root declares every rendered source (`slack`, `github`,
/// … — see `materialize_tng_root.sh`), so the positive cases browse
/// what is already there. The negative case is not in the fixture and
/// is the real shape: `media` is one of the download-only
/// providers, so a config for it genuinely has no `render_markdown`
/// step.
const GROUPS = `
[[groups]]
id = "media"
type = "media"

[[steps]]
group = "media"
function = "ingest"
`;

/// The painted values of one column of the search grid, retried until
/// the grid has them.
///
/// `allInnerTexts()` is a single sample. Waiting for the first row only
/// says the grid has started painting — cells fill a frame or more
/// later, so a read right after it can come back empty and the
/// assertion fails on a grid that is about to be right. Poll the
/// derived answer instead of the raw cells.
function columnValues(page: Page, colId: string) {
  return page.locator(`${SEARCH_ROWS} [col-id="${colId}"]`).allInnerTexts();
}

async function openManage(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await page.locator(ROWS).first().waitFor({ timeout: 10_000 });
}

async function writeConfig(page: Page, text: string): Promise<void> {
  await openManage(page);
  await page.locator(".m2-editor").fill(text);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
}

/// The Browse button on a group's row. Group rows carry `group:<id>` as
/// their row key (`groupRow` in the helpers), which beats matching on the
/// Name cell — that cell also renders the directory name beside the
/// label.
const groupRowOf = (page: Page, groupId: string) =>
  page.locator(`${ROWS}[data-key="group:${groupId}"]`);
const browseButton = (page: Page, groupId: string) =>
  groupRowOf(page, groupId).getByRole("button", { name: /^Browse/ });

/// Click Browse and wait until the grid card is actually up.
///
/// The search box belongs to the card and to nothing else, so it is
/// the honest signal that the navigation landed; the card's grid has
/// its own rows to wait on.
async function browse(page: Page, groupId: string, expectQuery: string) {
  await browseButton(page, groupId).click();
  await expect(page.locator(SEARCH)).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(SEARCH)).toHaveValue(expectQuery);
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 30_000 });
}

// Captured before this spec edits it and put back afterwards even on
// failure — leaving sources in the config would take later specs down.
let original = "";

test.beforeEach(async ({ page }) => {
  await openManage(page);
  original = await page.locator(".m2-editor").inputValue();
  await writeConfig(page, `${original.replace(/\s*$/, "")}\n${GROUPS}`);
});

test.afterEach(async ({ page }) => {
  if (original) await writeConfig(page, original);
});

test("a source's row opens that source, with its type's columns", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  await browse(page, "slack", "source_id:slack is:document");

  // The card stack IS the URL, which is what makes a browse
  // bookmarkable and shareable rather than a transient view.
  await expect(page).toHaveURL(/source_id%3Aslack/);

  // The card is named for what it holds, not for its query, and keeps
  // that name while the person searches inside it (checked below).
  const name = page.locator(".miller-col-title").last();
  await expect(name).toHaveText(/ documents$/);

  // Every row came from this source, and every row is a document: one
  // per thread, not the messages inside them. Both facts leave one
  // value in their column — Source, Kind — and the adaptive rule drops
  // a column with one value, so neither is on screen; the channel is,
  // and every one of them is a Slack channel or DM. The storage rows sit
  // in this source's directory but are filed under datalib, so a browse
  // of the source is the source's data — see docs/dev/grid_rows.md.
  await expect
    .poll(async () => {
      const channels = await columnValues(page, "channel");
      return channels.length > 0 && channels.every((c) => /^[#@]/.test(c.trim()));
    })
    .toBe(true);
  await expect(searchHeader(page, "kind")).toHaveCount(0);

  // The messages are one chip-delete away: drop `is:document` and the
  // Kind column earns its place back, with the thread's rows under it.
  await searchAndSettle(page, "source_id:slack");
  await expect
    .poll(async () => {
      const kinds = await columnValues(page, "kind");
      return kinds.length > 0 && kinds.every((k) => /^Slack /.test(k.trim()));
    })
    .toBe(true);
  expect(await columnValues(page, "kind")).toContain("Slack Message");
  await expect(name).toHaveText(/ documents$/);

  // Slack's preset: a channel and an author, and no Project — Slack has
  // no such thing, and a column of empty cells is what a preset exists
  // to prevent.
  await expect(searchHeader(page, "channel")).toBeVisible();
  await expect(searchHeader(page, "author")).toBeVisible();
  await expect(searchHeader(page, "project")).toHaveCount(0);
});

test("a different type gets a different column set", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  await browse(page, "github", "source_id:github is:document");

  // GitHub's preset is an author and the repo the row belongs to, and
  // no channel — the opposite pair to Slack's, from the same fixture.
  // Only `author` is asserted on screen: this library holds one
  // repository, so every row agrees on `project` and the adaptive rule
  // trims it. A preset is a ceiling, not a fixed set — and the two PRs
  // share an author too, so the column shows once the review comments
  // are back in the grid.
  await searchAndSettle(page, "source_id:github");
  await expect(searchHeader(page, "author")).toBeVisible();
  await expect(searchHeader(page, "channel")).toHaveCount(0);
});

test("the index group browses every source", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  // No filter: the index group's browse is every source at once.
  await browse(page, "unified_index", "");

  // The column that separates sources is the one that earns its place
  // here — the opposite of a per-source browse: the rows come from more
  // than one source, so the adaptive rule keeps it. Asked of the grid's
  // rows rather than the painted cells, which are only the few in view.
  // The query itself is already asserted by `browse` above.
  await expect
    .poll(async () => {
      const rows = await page.evaluate(() =>
        (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows(),
      );
      return new Set(rows.map((r) => r.source_id)).size;
    })
    .toBeGreaterThan(1);
  await expect(searchHeader(page, "source_ref")).toBeVisible();
});

test("a step's row opens its source, as its group's row does", async ({ page }) => {
  test.setTimeout(120_000);
  await openManage(page);
  await expandGroup(page, "slack");
  const step = pipelineRow(page, "slack/render_markdown").getByRole("button", {
    name: /^Browse/,
  });
  await expect(step).toBeEnabled();
  await step.click();
  await expect(page.locator(SEARCH)).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(SEARCH)).toHaveValue("source_id:slack is:document");
  await expect(page).toHaveURL(/source_id%3Aslack/);
});

/// A source that renders nothing has no rows at all — not even the
/// storage rows, which render is what emits. The button says so rather
/// than opening an empty grid onto a source that looks broken.
test("a download-only source cannot be browsed", async ({ page }) => {
  await openManage(page);
  const media = browseButton(page, "media");
  await expect(media).toBeDisabled();
  await expect(media).toHaveAttribute("title", /no render step/);
  // Its step says the same thing, not something of its own.
  await expandGroup(page, "media");
  const step = pipelineRow(page, "media/ingest").getByRole("button", { name: /^Browse/ });
  await expect(step).toBeDisabled();
  await expect(step).toHaveAttribute("title", /no render step/);
});
