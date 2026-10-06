import { test, expect } from "@playwright/test";
import {
  GRID,
  actOnRowByUuid,
  contextMenuRowByUuid,
  firstRowUuid,
  searchMenuItem,
  selectRowByUuid,
  stubClipboard,
  type GridApi,
} from "./grid-helpers";

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

  await page.goto(GRID);
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

/// The copy key on two selected rows: a TSV with the shown headers first,
/// and a stamp as the stamp rather than as the cell draws it.
test("the copy key puts the selected rows on the clipboard as TSV", async ({ page }) => {
  await page.goto(GRID);
  await firstRowUuid(page);
  const stamped = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi
      .rows()
      .filter((r) => typeof r.touched_at === "string")
      .slice(0, 2)
      .map((r) => ({ uuid: r.uuid as string, stamp: r.touched_at as string })),
  );
  expect(stamped, "the grid must hold two rows with a Touched stamp").toHaveLength(2);
  const [a, b] = stamped;

  await selectRowByUuid(page, a.uuid);
  await actOnRowByUuid(page, b.uuid, (row) =>
    row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  const readClipboard = await stubClipboard(page);
  await page.keyboard.press("ControlOrMeta+c");
  await expect.poll(readClipboard, { message: "nothing was copied" }).not.toBeNull();
  const copied = (await readClipboard())!;

  const headers = await page
    .locator(".grid-box .slick-header-column .slick-column-name")
    .allTextContents();
  expect(copied.split("\n")[0]).toBe(headers.map((h) => h.trim()).join("\t"));
  expect(copied).toContain(a.stamp);
  expect(copied).toContain(b.stamp);
});

/// A Touched cell draws the stamp in the viewer's zone; its copy is the
/// stamp as the row holds it.
test("a cell's right-click copies the cell's value", async ({ page }) => {
  await page.goto(GRID);
  await firstRowUuid(page);
  const row = await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi
      .rows()
      .find((r) => typeof r.touched_at === "string"),
  );
  expect(row, "the grid must hold a row with a Touched stamp").toBeTruthy();
  const readClipboard = await stubClipboard(page);

  await actOnRowByUuid(
    page,
    row!.uuid as string,
    (r) => r.locator('[col-id="touched_at"]').click({ button: "right", timeout: 3_000 }),
    "touched_at",
  );
  await expect(searchMenuItem(page, /^Copy Touched$/)).toBeVisible();
  await expect(searchMenuItem(page, /^Copy$/)).toHaveCount(0);
  await searchMenuItem(page, /^Copy Touched$/).click();
  await expect
    .poll(readClipboard, { message: "clipboard after Copy Touched" })
    .toBe(row!.touched_at);
});
