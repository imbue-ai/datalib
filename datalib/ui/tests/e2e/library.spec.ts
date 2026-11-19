import { test, expect, type Page } from "@playwright/test";

// The Library card on the TNG root (cards/LibraryCard.ce.vue): the sources
// as a tree, their places a level at a time from `/children?places=true`,
// and the place opened there shown beside it as a node card, its list
// read beside a preview. What is open survives a reload.

const treeRow = (page: Page, text: string) =>
  page.locator("vs-tree .monaco-list-row", { hasText: text }).first();
const nodeRows = (page: Page) => page.locator("[data-testid=node-children] .nc-row");

async function openLibrary(page: Page) {
  await page.goto("/" + encodeURIComponent("libraryTreeView()"));
  await expect(treeRow(page, "slack")).toBeVisible();
}

test("a channel opened in the tree is listed beside it, a thread read in place", async ({
  page,
}) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  await expect(treeRow(page, "#bridge")).toBeVisible();
  // A thread is no place: the channel has nothing below it in the tree.
  await expect(treeRow(page, "Slack Thread")).toHaveCount(0);

  await treeRow(page, "#bridge").dblclick();
  await expect(page.locator(".lib-pane-title")).toHaveText("#bridge");
  // A channel's threads are each named for the channel; listed in it,
  // each reads as its first line.
  await expect(nodeRows(page).filter({ hasText: "Shields up." })).toHaveCount(1);
  await expect(nodeRows(page).filter({ hasText: /^#bridge/ })).toHaveCount(0);

  await nodeRows(page).last().click();
  await expect(page.locator(".nc-preview-body .doc-bar-title")).toBeVisible();
  await expect(page.locator(".nc-preview-body .doc-bar-title")).toContainText("#bridge");
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
  await treeRow(page, "Inbox").dblclick();
  await expect(page.locator(".lib-pane-title")).toHaveText("Inbox");
  await expect(nodeRows(page).filter({ hasText: "Mission Briefing" })).toHaveCount(1);
});

test("a Notion page is a place in the tree, and shows its own text beside it", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "notion").click();
  await treeRow(page, "Bridge Operations Handbook").click();
  // A page under a page is a place as Notion's sidebar has it.
  await expect(treeRow(page, "Shift Roster")).toBeVisible();
  await treeRow(page, "Shift Roster").dblclick();
  await expect(page.locator(".lib-pane-title")).toHaveText("Shift Roster");
  // It holds nothing below it, so its own text is all the pane shows.
  await expect(page.locator(".nc-preview-body .doc-bar-title")).toContainText("Shift Roster");
  await expect(page.locator("[data-testid=node-children]")).toBeHidden();
});

test("what was open in the tree and beside it comes back after a reload", async ({ page }) => {
  await openLibrary(page);
  await treeRow(page, "slack").click();
  await treeRow(page, "Slack Workspace").click();
  await treeRow(page, "#engineering").dblclick();
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
