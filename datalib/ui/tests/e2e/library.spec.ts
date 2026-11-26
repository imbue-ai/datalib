import { test, expect, type Page } from "@playwright/test";

// The Library card on the TNG root (cards/LibraryCard.ce.vue): the sources
// as a tree, their places a level at a time from `/places`,
// each drawn as its chip, and the place picked there shown beside it as a
// node card: the search list held to it, with its preview. What is open
// survives a reload.

const treeRow = (page: Page, text: string) =>
  page.locator("vs-tree .monaco-list-row", { hasText: text }).first();
// The Library card's own pane, not a node card opened from it.
const pane = (page: Page) => page.locator("[data-testid=library-node]");
const nodeRows = (page: Page) => pane(page).locator(".sc-result");
const previewTitle = (page: Page) => pane(page).locator(".sc-preview-body .doc-bar-title");
const chipMenu = (page: Page) => page.locator(".chip-menu");

async function openLibrary(page: Page) {
  await page.goto("/" + encodeURIComponent("libraryTreeView()"));
  await expect(treeRow(page, "slack")).toBeVisible();
}

// `<vs-tree>` draws nothing until it has its first level, so a slow answer
// once left the card blank with nothing to say why.
test("the tree says it is loading until its sources answer, and why when they cannot", async ({
  page,
}) => {
  let release = () => {};
  const released = new Promise<void>((resolve) => (release = resolve));
  const top = "**/applet/unified_grid_and_search_index/sources_with_places";
  let asked = 0;
  await page.route(top, async (route) => {
    asked++;
    await released;
    await route.continue();
  });
  await page.goto("/" + encodeURIComponent("libraryTreeView()"));
  const loading = page.locator("[data-testid=library-loading]");
  await expect(loading).toBeVisible();
  release();
  await expect(treeRow(page, "slack")).toBeVisible();
  await expect(loading).toHaveCount(0);
  expect(asked, "one request tells which sources open").toBe(1);

  await page.unroute(top);
  await page.route(top, (route) => route.fulfill({ status: 500, body: "no" }));
  await page.reload();
  await expect(page.locator(".lib-failed")).toContainText("The library could not be read");
  await expect(loading).toHaveCount(0);
});

test("a channel opened in the tree is listed beside it, a thread read in place", async ({
  page,
}) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  await expect(treeRow(page, "#bridge")).toBeVisible();
  // A thread is no place: the channel has nothing below it in the tree.
  await expect(treeRow(page, "Slack Thread")).toHaveCount(0);

  await treeRow(page, "#bridge").click();
  await expect(page.locator(".lib-pane-title")).toHaveText("#bridge");
  // A channel's threads are each named for the channel; listed in it,
  // each reads as its first line.
  await expect(nodeRows(page).filter({ hasText: "Shields up." })).toHaveCount(1);
  await expect(nodeRows(page).filter({ hasText: /^#bridge/ })).toHaveCount(0);

  // The newest is read beside the list at once, and another when picked.
  await expect(nodeRows(page).first()).toHaveClass(/is-picked/);
  await nodeRows(page).last().click();
  await expect(nodeRows(page).last()).toHaveClass(/is-picked/);
  await expect(previewTitle(page)).toContainText("#bridge");

  // The keyboard stays in the tree: the pane's box does not take it.
  await treeRow(page, "#engineering").click();
  await expect(page.locator(".lib-pane-title")).toHaveText("#engineering");
  await expect(nodeRows(page).first()).toBeVisible();
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("Enter");
  await expect(page.locator(".lib-pane-title")).not.toHaveText("#engineering");
});

test("what is typed in a place's box searches within it", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  // The channels are in the workspace; their threads are in them.
  await expect(pane(page)).toContainText("Nothing is directly in here");
  await treeRow(page, "#bridge").click();
  await expect(nodeRows(page).filter({ hasText: "Shields up." })).toHaveCount(1);
  const all = await nodeRows(page).count();
  const box = pane(page).getByTestId("grid-search-input");
  // The place is the card's, so it is not among the words to edit.
  await expect(box).toHaveText("is:document");
  await box.click();
  await page.keyboard.press("End");
  await page.keyboard.type(' "shields up"');
  await expect(nodeRows(page)).toHaveCount(1);
  expect(all).toBeGreaterThan(1);
  await expect(nodeRows(page)).toContainText("Shields up.");
});

test("a mailbox's labels are places, and a label lists the threads filed under it", async ({
  page,
}) => {
  await openLibrary(page);
  // A folder of PDFs opens onto its folders; holodeck/ holds nothing
  // rendered, so it is no place.
  await treeRow(page, "tng_pdfs").click();
  await expect(treeRow(page, "engineering")).toBeVisible();
  await expect(treeRow(page, "archive")).toBeVisible();
  await expect(treeRow(page, "holodeck")).toHaveCount(0);
  await treeRow(page, "tng_email").click();
  await treeRow(page, "Email Account").click();
  await expect(treeRow(page, "Inbox")).toBeVisible();
  await treeRow(page, "Inbox").click();
  await expect(page.locator(".lib-pane-title")).toHaveText("Inbox");
  await expect(nodeRows(page).filter({ hasText: "Mission Briefing" })).toHaveCount(1);
});

test("a Notion page is a place in the tree, and shows its own text beside it", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "notion").click();
  await treeRow(page, "Bridge Operations Handbook").click();
  // A page under a page is a place as Notion's sidebar has it.
  await expect(treeRow(page, "Shift Roster")).toBeVisible();
  await treeRow(page, "Shift Roster").click();
  await expect(page.locator(".lib-pane-title")).toHaveText("Shift Roster");
  // It holds nothing below it, so its own text is all the pane shows.
  await expect(previewTitle(page)).toContainText("Shift Roster");
  await expect(pane(page).getByRole("list", { name: "Results" })).toBeHidden();

  // A page with a page in it: both are listed, its own text read first.
  await treeRow(page, "Bridge Operations Handbook").click();
  await expect(nodeRows(page).filter({ hasText: "Shift Roster" })).not.toHaveCount(0);
  await expect(previewTitle(page)).toContainText("Bridge Operations Handbook");
});

test("what was open in the tree and beside it comes back after a reload", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  await treeRow(page, "#engineering").click();
  await expect(page.locator(".lib-pane-title")).toHaveText("#engineering");
  // The card saves its state a frame after the change, and the layout
  // keeps it (in this window's session storage, as the suite runs it).
  await expect
    .poll(() =>
      page.evaluate(() => Object.values(sessionStorage).some((v) => v.includes("engineering"))),
    )
    .toBe(true);

  await page.reload();
  await expect(treeRow(page, "#engineering")).toBeVisible();
  await expect(page.locator(".lib-pane-title")).toHaveText("#engineering");
  await expect(nodeRows(page).first()).toBeVisible();
});

test("a source is its chip, with its Sources row's menu", async ({ page }) => {
  await openLibrary(page);
  const slack = treeRow(page, "slack").locator("a.chip[data-entity='datalib:group/slack']");
  await expect(slack).toBeVisible();
  // The row's entries come with what the server says of the source; until
  // then the menu holds the chip's own.
  await expect(async () => {
    await page.keyboard.press("Escape");
    await slack.click({ button: "right" });
    await expect(chipMenu(page)).toContainText("Sync now", { timeout: 1000 });
  }).toPass();
  await page.keyboard.press("Escape");
  await expect(chipMenu(page)).toHaveCount(0);
});

test("a place's chip opens its own card", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  const engineering = treeRow(page, "#engineering").locator("a.chip[data-node]");
  await engineering.click({ button: "right" });
  await expect(chipMenu(page).locator(".chip-menu-item")).toContainText([
    "Open “#engineering”",
    "Copy ID",
  ]);
  await page.keyboard.press("Escape");
  await engineering.dblclick();
  await expect(page.locator('.ct-card[data-card-source^="nodeView("]')).toHaveCount(1);
  await expect(page.locator(".ct-card-title").filter({ hasText: /^#engineering$/ })).toHaveCount(1);
});
