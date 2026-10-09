import { test, expect } from "@playwright/test";
import { cardOf, GRID, SHOWN_CARDS, shownCards, tabLabels } from "./grid-helpers";

// A URL naming cards — a link, a popped-out card, a `/chat/<uuid>` link
// every renderer writes into a document body — opens those cards as a
// tab of their own. What is open is kept in the layout, not the URL, so
// the address goes back to "/".

// In the tab shown: a tab switched away from keeps its cards mounted.
const chatPreview = `${SHOWN_CARDS} .chat-preview`;

test("a URL naming cards opens them as a tab, and the address goes back to /", async ({ page }) => {
  await page.goto(GRID);
  await expect(cardOf(page, "searchView()")).toHaveCount(1);
  // The three pinned tabs, then the grid's, which also calls itself Search.
  await expect(tabLabels(page)).toHaveText(["Dashboard", "Search", "Sources", "Search"]);
  await expect.poll(() => page.evaluate(() => location.pathname)).toBe("/");
});

test("the page title names the tab shown", async ({ page }) => {
  await page.goto(GRID);
  await expect(page).toHaveTitle("Search · Datalib");
  await tabLabels(page).first().click();
  await expect(page).toHaveTitle("Dashboard · Datalib");
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
    await expect(cardOf(page, `documentView("${md}")`)).toHaveCount(1);
    await expect(shownCards(page)).toHaveCount(1);
  }
});
