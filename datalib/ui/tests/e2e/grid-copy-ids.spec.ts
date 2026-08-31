import { test, expect } from "@playwright/test";
import { contextMenuRowByUuid, stubClipboard } from "./grid-helpers";

// Two copy actions, two id spaces, and the user must be able to tell
// which one they got.
//
// `grid_rows.uuid` is ours: it resolves inside datalib (chat URLs,
// `feedback.target_uuids`, `id:` filters). `source_native_id` is the
// upstream's: it resolves at claude.ai, in the GitHub API, in
// `conversations.replies`. They are usually BOTH UUID-ish strings, so
// nothing about the copied text tells you which space it belongs to —
// which is why this is two menu items rather than one action that
// returns whichever exists.
//
// The regression this pins is specific. Before `datalib_id`, anthropic,
// chatgpt and notion passed the upstream id straight through as their
// primary key, so "Copy UUID(s)" happened to yield a native id for
// those three and ours for the other thirteen. Nothing in the code
// chose that; it fell out of the schema. Porting those providers onto
// minted v5 ids silently turns `uuid` into a value with no route back
// upstream, and without a second action the native id becomes
// unreachable from the UI entirely.

type Row = {
  uuid: string;
  source_native_id: string;
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
  return page.locator(".ag-menu .ag-menu-option").filter({ hasText: name });
}

test("a row with an upstream id offers both copies, and they differ", async ({
  page,
  request,
}) => {
  const all = await rows(request);
  // A row whose native id is genuinely NOT its uuid — otherwise the
  // "they differ" assertion below could pass for the wrong reason on a
  // provider that still passes the upstream id through.
  const row = all.find(
    (r) => r.source_native_id && r.source_native_id !== r.uuid,
  );
  expect(
    row,
    "fixture must contain a row whose source_native_id differs from its uuid " +
      "(slack threads carry `{channel_id}:{ts}` against a v5 uuid)",
  ).toBeTruthy();

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  const readClipboard = await stubClipboard(page);
  await contextMenuRowByUuid(page, row!.uuid);

  await expect(menuItem(page, /^Copy UUID$/)).toBeVisible();
  await expect(menuItem(page, /^Copy source ID$/)).toBeVisible();

  await menuItem(page, /^Copy source ID$/).click();
  await expect
    .poll(readClipboard, { message: "clipboard after Copy source ID" })
    .toBe(row!.source_native_id);

  // ...and the other action still yields OUR id, not the upstream one.
  await contextMenuRowByUuid(page, row!.uuid);
  await menuItem(page, /^Copy UUID$/).click();
  await expect
    .poll(readClipboard, { message: "clipboard after Copy UUID" })
    .toBe(row!.uuid);
});

test("a row with no upstream id hides the source-id action", async ({
  page,
  request,
}) => {
  const all = await rows(request);
  // Message-level rows carry no native id yet — per-item ids land with
  // the per-provider `datalib_id` port. When that lands and every row
  // has one, this test should start failing to find a subject; delete
  // it then rather than weakening it.
  const row = all.find((r) => !r.source_native_id);
  expect(
    row,
    "fixture must contain a row with no source_native_id",
  ).toBeTruthy();

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  await contextMenuRowByUuid(page, row!.uuid);
  await expect(menuItem(page, /^Copy UUID$/)).toBeVisible();
  // A menu item that silently copies nothing is worse than no item.
  await expect(menuItem(page, /^Copy source ID$/)).toHaveCount(0);
});
