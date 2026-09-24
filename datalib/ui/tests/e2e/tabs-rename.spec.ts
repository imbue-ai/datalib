// The tabs layout, the app's default: a tab is named by its card until
// the person renames it, and a card's requests say which card made them.

import { test, expect, type Page } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("datalib-layout", "tabs"));
});

const firstLabel = (page: Page) => page.locator(".tabs-row .tabs-label").first();
const nameBox = (page: Page) => page.getByRole("textbox", { name: "tab name" });

async function search(page: Page, q: string) {
  const card = page.locator(".tabs-main");
  await card.getByTestId("search-input").fill(q);
  await expect(card.locator(".grid-wrap")).toHaveAttribute("data-shown-query", q);
}

async function renameFromMenu(page: Page) {
  await page.locator(".tabs-row").first().click({ button: "right" });
  await page.getByRole("menuitem", { name: "Rename…" }).click();
  await expect(nameBox(page)).toBeFocused();
}

test("a card's requests name the card and its type", async ({ page }) => {
  const search = page.waitForRequest((r) => r.url().includes("/applet/unified_index/search"));
  await page.goto("/");
  const headers = (await search).headers();
  expect(headers["x-datalib-card"]).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab]/);
  expect(headers["x-datalib-card-type"]).toBe("gridView");
});

test("a renamed tab keeps its name through a search and a reload", async ({ page }) => {
  await page.goto("/");
  await expect(firstLabel(page)).toHaveText(/^Search/);

  // Unrenamed, the grid names its tab after the query.
  await search(page, "kraken");
  await expect(firstLabel(page)).toHaveText("Search: kraken");

  await renameFromMenu(page);
  await nameBox(page).fill("Bridge chatter");
  await nameBox(page).press("Enter");
  await expect(firstLabel(page)).toHaveText("Bridge chatter");
  await expect(page).toHaveTitle(/^Bridge chatter/);

  // The card no longer names the tab once the person has.
  await search(page, "warp");
  await expect(firstLabel(page)).toHaveText("Bridge chatter");

  await page.reload();
  await expect(firstLabel(page)).toHaveText("Bridge chatter");
});

test("Escape and a blank name leave the tab as it was", async ({ page }) => {
  await page.goto("/");
  await expect(firstLabel(page)).toHaveText(/^Search/);
  const before = await firstLabel(page).innerText();

  await renameFromMenu(page);
  await nameBox(page).fill("Not this");
  await nameBox(page).press("Escape");
  await expect(nameBox(page)).toHaveCount(0);
  await expect(firstLabel(page)).toHaveText(before);

  // A double-click on the name is the other way in.
  await firstLabel(page).dblclick();
  await nameBox(page).fill("   ");
  await nameBox(page).press("Enter");
  await expect(firstLabel(page)).toHaveText(before);
});
