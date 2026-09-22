import { test, expect } from "@playwright/test";
import { contextMenuRowByUuid, stubClipboard } from "./grid-helpers";

// Two copy actions, two id spaces, and the user must be able to tell
// which one they got.

type Row = {
  uuid: string;
  upstream_id: string;
  provider: string;
  kind: string;
};

async function rows(request: import("@playwright/test").APIRequestContext) {
  const resp = await request.get("/applet/unified_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: Row[] };
  expect(data.rows.length, "fixture must have rows").toBeGreaterThan(0);
  return data.rows;
}

function menuItem(page: import("@playwright/test").Page, name: RegExp) {
  // The item's text, beside its icon slot — which is a bullet character
  // when the item has no icon, and so part of the item's own text.
  return page.locator(".slick-context-menu .slick-menu-content").filter({ hasText: name });
}

test("a row with an upstream id offers both copies, and they differ", async ({ page, request }) => {
  const all = await rows(request);
  // A row whose native id is genuinely NOT its uuid — otherwise the
  // "they differ" assertion below could pass for the wrong reason on a
  // provider that still passes the upstream id through.
  const row = all.find((r) => r.upstream_id && r.upstream_id !== r.uuid);
  expect(
    row,
    "fixture must contain a row whose upstream_id differs from its uuid " +
      "(slack messages carry `{team}#{channel}#{ts}` against a datalib uuid)",
  ).toBeTruthy();

  await page.goto("/");
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });

  const readClipboard = await stubClipboard(page);
  await contextMenuRowByUuid(page, row!.uuid);

  await expect(menuItem(page, /^Copy UUID$/)).toBeVisible();
  await expect(menuItem(page, /^Copy upstream ID$/)).toBeVisible();

  await menuItem(page, /^Copy upstream ID$/).click();
  await expect
    .poll(readClipboard, { message: "clipboard after Copy upstream ID" })
    .toBe(row!.upstream_id);

  // ...and the other action still yields OUR id, not the upstream one.
  await contextMenuRowByUuid(page, row!.uuid);
  await menuItem(page, /^Copy UUID$/).click();
  await expect.poll(readClipboard, { message: "clipboard after Copy UUID" }).toBe(row!.uuid);
});
