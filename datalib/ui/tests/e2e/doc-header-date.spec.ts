import { test, expect } from "@playwright/test";
import { selectRowByUuid, docBody, EVERY_ROW } from "./grid-helpers";

// The document header printed the raw `created_at` right above the first
// message, which shows the same moment in its own short form (#902). The
// date belongs to the messages; the header carries none.

test("the document header does not repeat the raw created_at", async ({ page, request }) => {
  const resp = await request.get("/applet/unified_index/search?q=&limit=1000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as {
    rows: {
      uuid: string;
      markdown_uuid: string | null;
      kind: string;
      message_index: number | null;
    }[];
  };
  const pick = data.rows.find(
    (r) => r.kind !== "Chat" && r.message_index != null && r.markdown_uuid,
  );
  expect(pick, "fixture must contain a message row").not.toBeUndefined();

  const chat = await request.get(`/applet/unified_index/chat/${pick!.markdown_uuid}`);
  expect(chat.ok()).toBeTruthy();
  const { created_at } = (await chat.json()) as { created_at: string | null };
  expect(created_at, "the picked document must have a created_at to look for").toBeTruthy();

  await page.goto(EVERY_ROW);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });
  await selectRowByUuid(page, pick!.uuid);

  const card = page.locator(`.chat-preview[data-markdown-uuid="${pick!.markdown_uuid}"]`);
  // The selected message's own stamp being drawn means the document has
  // loaded, so the header below is the real one and not an empty
  // placeholder. Not the document's first stamp: that can sit in a
  // version of the conversation the page folds away.
  await expect(
    docBody(card).locator(`[data-section-uuid="${pick!.uuid}"] .msg-ts`).first(),
  ).toBeVisible({ timeout: 10_000 });
  const header = card.locator(".chat-header");
  await expect(header).toBeVisible();
  await expect(header).not.toContainText(created_at!);
});
