import { test, expect } from "@playwright/test";
import { firstRowUuid, selectRowByUuid, type GridApi } from "./grid-helpers";

// The miller stack rides the browser's history. Opening a column is a
// navigation: Back closes it and Forward reopens it, and the columns
// the two stacks share stay mounted — the grid beside a document is
// not rebuilt when the document goes. A card's state (the grid's
// selection) rewrites the current entry instead of adding one. The
// page's title names the stack, and a `/chat/<uuid>` link — the shape
// every renderer writes into a document body — is a page of its own.

const chatPreview = ".chat-preview";

// A mark on the grid's column element: still there after a navigation
// only if the element survived it.
async function markGridColumn(page: import("@playwright/test").Page) {
  await page
    .locator(".miller-col")
    .first()
    .evaluate((el) => {
      (el as HTMLElement).dataset.mark = "kept";
    });
}
const gridMark = (page: import("@playwright/test").Page) =>
  page.locator(".miller-col").first().getAttribute("data-mark");

async function isSelected(page: import("@playwright/test").Page, uuid: string) {
  return page.evaluate(
    (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
    uuid,
  );
}

test("Back closes the column a click opened; Forward reopens it", async ({ page }) => {
  await page.goto("/");
  const rowId = await firstRowUuid(page);
  await markGridColumn(page);

  await selectRowByUuid(page, rowId);
  await expect(page.locator(chatPreview)).toBeVisible();
  const withDoc = await page.evaluate(() => location.pathname);

  await page.goBack();
  await expect(page.locator(chatPreview)).toHaveCount(0);
  // The entry Back returned to carries the selection the click made,
  // so the grid is the same element, still on that row.
  expect(await gridMark(page)).toBe("kept");
  expect(await isSelected(page, rowId)).toBe(true);
  expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toContain(
    `sel=${rowId}`,
  );

  await page.goForward();
  await expect(page.locator(chatPreview)).toBeVisible();
  expect(await page.evaluate(() => location.pathname)).toBe(withDoc);
  expect(await gridMark(page)).toBe("kept");
});

test("closing a column is a navigation Back undoes", async ({ page }) => {
  await page.goto("/");
  const rowId = await firstRowUuid(page);
  await selectRowByUuid(page, rowId);
  await expect(page.locator(chatPreview)).toBeVisible();

  await page
    .locator(".miller-col", { has: page.locator(chatPreview) })
    .locator(".card-control--close")
    .click();
  await expect(page.locator(chatPreview)).toHaveCount(0);

  await page.goBack();
  await expect(page.locator(chatPreview)).toBeVisible();
});

test("the page title names the stack, newest column first", async ({ page }) => {
  await page.goto("/");
  await expect(page).toHaveTitle("Search · Datalib");
  const rowId = await firstRowUuid(page);
  await selectRowByUuid(page, rowId);
  await expect(page.locator(chatPreview)).toBeVisible();
  // The document card retitles itself once its fetch lands.
  await expect(page).toHaveTitle(/^(?!Document ·).+ · Search · Datalib$/);
});

test("a /chat/<uuid> link is the document alone", async ({ page, request }) => {
  const resp = await request.get("/applet/unified_index/search?q=&limit=20");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { markdown_uuid: string | null }[] };
  const md = rows.find((r) => r.markdown_uuid)?.markdown_uuid;
  expect(md, "fixture must have a row with a rendered document").toBeTruthy();

  for (const href of [`/chat/${md}`, `/#/chat/${md}`]) {
    await page.goto(href);
    await expect(page.locator(chatPreview)).toBeVisible({ timeout: 10_000 });
    expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toBe(
      `/documentView("${md}")`,
    );
    await expect(page.locator(".miller-col")).toHaveCount(1);
  }
});
