import { test, expect } from "@playwright/test";
import {
  EVERY_ROW,
  SEARCH_MENU,
  cardOf,
  gridSettled,
  inDocFrame,
  searchAndSettle,
  searchMenuItem,
  selectRowByUuid,
  stubClipboard,
} from "./grid-helpers";

// The search grid's Author cell is the same chip a document draws
// (docs/dev/chips.md § "In a grid"): a link with the handle, the
// name the source showed, the kind's mark. Right-click on it offers the
// chip's entries ahead of the row's; a double-click opens the person's
// card, led by the row's source. The fixture has no contacts app, so the
// chips are unresolved; the shape and the clicks are what every root has.

type Row = { uuid: string; author_handle: string | null; author_ref: { id: string } | null };

// Slack's authors all carry a handle, and the grid draws only the rows in
// view, so the grid is opened on Slack rather than on everything.
const SLACK_ROWS = "/searchView()::q%3Dsource_id%3Aslack";

async function anAuthorChip(page: import("@playwright/test").Page) {
  await page.goto(SLACK_ROWS);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });
  const chip = page.locator(".grid-box .slick-cell a.chip[data-handle]").first();
  await expect(chip).toBeVisible({ timeout: 10_000 });
  return chip;
}

test("an author with a handle is drawn as a chip link", async ({ page, request }) => {
  const resp = await request.get("/applet/unified_index/search?q=source_id%3Aslack&limit=200");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const withHandle = rows.find((r) => r.author_handle);
  expect(withHandle, "the fixture must have a row whose author has a handle").toBeTruthy();
  // The applet sends the handle as the identity's id, as a URI.
  expect(withHandle!.author_ref?.id).toMatch(/^(mailto:|tel:|slack:)/);

  const chip = await anAuthorChip(page);
  await expect(chip).toHaveAttribute("href", /^(mailto:|tel:|slack:)/);
  await expect(chip).toHaveClass(/handle-unresolved/);
  await expect(chip).not.toHaveText("");
});

test("right-click on the chip offers its copies, and a double-click opens the person", async ({
  page,
}) => {
  const chip = await anAuthorChip(page);
  const handle = (await chip.getAttribute("data-handle"))!;
  const value = handle.slice(handle.indexOf(":") + 1);
  await stubClipboard(page);

  await chip.click({ button: "right" });
  await expect(page.locator(SEARCH_MENU)).toBeVisible({ timeout: 5_000 });
  await searchMenuItem(page, `Copy ${value}`).click();
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied))
    .toBe(value);

  const bar = page.locator('[data-testid="search-input"]');
  const query = (await bar.getAttribute("data-query")) ?? "";
  await chip.dblclick();
  const card = page.locator(".person");
  await expect(card.locator(".person-name")).not.toHaveText("", { timeout: 10_000 });
  await expect(card.locator(".person-section").first()).toHaveClass(/person-seen-here/);
  // The grid's own search is left as it was.
  await expect(bar).toHaveAttribute("data-query", query);
});

/// The Source cell is a group chip (docs/dev/chips.md): datalib-http
/// answers what the group is now — its name, its type, its status — the
/// menu copies its id, and a double-click opens its sync dashboard.
test("a row's Source is a group chip that resolves, copies its id and opens its dashboard", async ({
  page,
}) => {
  await page.goto(SLACK_ROWS);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });
  const chip = page
    .locator('.grid-box .slick-cell a.chip[data-entity="datalib:group/slack"]')
    .first();
  await expect(chip).toBeVisible({ timeout: 10_000 });
  // Resolved: the hover goes past the name to what the group is and its
  // status, which only `/api/entities` knows.
  await expect(chip).toHaveAttribute("title", /\(slack\)\n.+\n.+/, { timeout: 10_000 });

  await stubClipboard(page);
  await chip.click({ button: "right" });
  await expect(page.locator(SEARCH_MENU)).toBeVisible({ timeout: 5_000 });
  await searchMenuItem(page, "Copy slack").click();
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied))
    .toBe("slack");

  await chip.dblclick();
  await expect(cardOf(page, 'syncDashboardView({"group":"slack"})')).toBeVisible({
    timeout: 10_000,
  });
});

/// A source's storage report names its group in its heading and each
/// store's step in its own column, as chips: they resolve to what the
/// config calls them now, and a double-click opens the group's dashboard
/// or the step's log.
test("a storage report's group and step are chips that open their cards", async ({
  page,
  request,
}) => {
  const resp = await request.get(
    `/applet/unified_index/search?q=${encodeURIComponent("source_id:datalib")}&limit=500`,
  );
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { uuid: string; markdown_uuid: string | null; conversation_name: string }[];
  };
  const report = rows.find((r) => r.conversation_name === "slack storage" && r.markdown_uuid);
  expect(report, "the fixture must have the slack storage report").toBeTruthy();
  await page.goto(EVERY_ROW);
  await searchAndSettle(page, "source_id:datalib");
  await gridSettled(page);
  await selectRowByUuid(page, report!.uuid);

  const group = (await inDocFrame(page, 'a.chip[data-entity="datalib:group/slack"]')).first();
  await expect(group).toHaveAttribute("title", /\(slack\)\n.+/, { timeout: 10_000 });
  // The report names the step its store sits under, `slack/ingest`. This
  // fixture root declares its sources render-only, so no such step is in
  // its config and the chip stays as written: the step's name and id,
  // nothing resolved. It still opens the step's log.
  const step = (await inDocFrame(page, 'a.chip[data-entity="datalib:step/slack/ingest"]')).first();
  await expect(step).toHaveAttribute("title", "ingest (slack/ingest)");

  // The document's right-click menu on a group chip is the chip's.
  await stubClipboard(page);
  await group.click({ button: "right" });
  await page.locator(".chip-menu .chip-menu-item", { hasText: /^Copy slack$/ }).click();
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied))
    .toBe("slack");

  await group.dblclick();
  await expect(cardOf(page, 'syncDashboardView({"group":"slack"})')).toBeVisible({
    timeout: 10_000,
  });
  await step.dblclick();
  await expect(cardOf(page, 'logView({"step":"slack/ingest"')).toBeVisible({ timeout: 10_000 });
});
