import { test, expect } from "@playwright/test";

// Bug #2: clicking a grid row should scroll the markdown panel on the
// right to the corresponding message — even when the chat is already
// loaded.
//
// Currently every message-row in the grid carries message_index = 0, so
// the panel always tries to scroll to the first message and never moves.
// The test pins the contract: selecting two different message-rows must
// result in different preview-pane scroll positions.

test("row click scrolls the chat preview pane", async ({ page }) => {
  await page.goto("/");

  // All rows are returned by default. Filter the grid view to message
  // rows (kind != "Chat"). The "Type" column shows "User Input" /
  // "LLM Response" / "Tool Call" — Chat rows show "Chat".
  const messageRows = page
    .locator('.ag-center-cols-container [role="row"]')
    .filter({ hasNotText: /^.*\bChat\b.*$/ });
  await expect(messageRows.first()).toBeVisible({ timeout: 10_000 });
  const count = await messageRows.count();
  expect(count, "fixture should surface message rows").toBeGreaterThanOrEqual(2);

  // First click — any message row. Wait for the preview panel to render.
  await messageRows.first().click();
  const preview = page.locator(".chat-preview");
  await expect(preview.locator(".message").first()).toBeVisible({
    timeout: 10_000,
  });

  // Pick a second row that points at a different message within the
  // *same* chat as the first one. We do this by reading conversation
  // UUIDs out of the JSON the SearchView fetched, exposed indirectly via
  // the AG Grid row identity. The simplest browser-side proof: walk the
  // visible message rows in order and choose the latest one whose snippet
  // text differs from the first row's snippet.
  const firstSnippet = await messageRows.first().textContent();
  let secondIdx = -1;
  for (let i = 1; i < count; i++) {
    const t = await messageRows.nth(i).textContent();
    if (t && t !== firstSnippet) {
      secondIdx = i;
      break;
    }
  }
  expect(secondIdx, "needed a distinct second message row").toBeGreaterThan(0);

  const scrollAfterFirst = await preview.evaluate((el) => el.scrollTop);

  await messageRows.nth(secondIdx).click();
  // Allow the conversationUuid / messageIndex watchers + scrollIntoView.
  await page.waitForTimeout(300);
  await expect(preview.locator(".message").first()).toBeVisible();

  const scrollAfterSecond = await preview.evaluate((el) => el.scrollTop);

  // The two clicks targeted different messages, so the scroll position
  // must differ. Today both end up at 0 because every grid row carries
  // message_index = 0.
  expect(
    scrollAfterSecond,
    `clicking a different message row should change preview scroll ` +
      `(was ${scrollAfterFirst}, after = ${scrollAfterSecond})`,
  ).not.toBe(scrollAfterFirst);
});
