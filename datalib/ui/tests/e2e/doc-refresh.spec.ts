// An open document follows the index. Its body and its problems banner
// are both read from the index, and a card that fetched them once kept
// showing an error a later render had already cleared.
//
// The live stream is stubbed here: nothing in the fixture commits to
// the index on cue, and the frame is all the card listens for.

import { test, expect, type Page } from "@playwright/test";
import { docBody, EVERY_ROW, SEARCH_ROWS, selectRowByUuid } from "./grid-helpers";

const PROBLEM = {
  problem_uuid: "00000000-0000-4000-8000-00000000d0c5",
  severity: "error",
  stage: "render",
  outcome: "dropped",
  reason: "render_failed",
  field: null,
  rule: null,
  sample: "",
  item_uuid: null,
  first_seen_at_utc: "2026-01-01T00:00:00Z",
};

/// Serve the live stream as one frame per connection, reconnecting fast:
/// heartbeats until `commit()`, an index commit after it.
async function stubStream(page: Page): Promise<() => void> {
  let committed = false;
  await page.route("**/api/sync/stream", (route) => {
    const frame = committed ? { kind: "index_changed" } : { kind: "heartbeat" };
    return route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
      body: `retry: 200\nevent: root\ndata: ${JSON.stringify(frame)}\n\n`,
    });
  });
  return () => {
    committed = true;
  };
}

test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

test("an index commit clears a problem the open document no longer has", async ({ page }) => {
  const resp = await page.request.get("/applet/unified_index/search?q=&limit=1000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { uuid: string; markdown_uuid: string | null; message_index: number | null }[];
  };
  const pick = rows.find((r) => r.message_index != null && r.markdown_uuid);
  expect(pick, "the fixture has a message row").toBeDefined();

  const commit = await stubStream(page);
  let stale = true;
  await page.route(`**/applet/unified_index/chat/${pick!.markdown_uuid}`, async (route) => {
    const response = await route.fetch();
    const body = (await response.json()) as { problems?: unknown[] };
    if (stale) body.problems = [PROBLEM, ...(body.problems ?? [])];
    await route.fulfill({ response, json: body });
  });

  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await selectRowByUuid(page, pick!.uuid);
  const card = page.locator(`.chat-preview[data-markdown-uuid="${pick!.markdown_uuid}"]`);
  await expect(docBody(card)).toBeVisible();
  const banner = card.locator(`[data-problem-uuid="${PROBLEM.problem_uuid}"]`);
  await expect(banner).toBeVisible();

  stale = false;
  commit();
  await expect(banner).toHaveCount(0, { timeout: 10_000 });
  await expect(docBody(card)).toBeVisible();
});
