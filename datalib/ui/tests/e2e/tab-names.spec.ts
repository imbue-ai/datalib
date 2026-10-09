// A tab is named by its card until the person renames it, and a card's
// requests say which card made them.

import { test, expect, type Page } from "@playwright/test";
import { GRID, tabLabels, typeInto } from "./grid-helpers";

// The tab GRID opens, after the three pinned ones.
const gridTab = (page: Page) => tabLabels(page).nth(3);
const nameBox = (page: Page) => page.getByLabel("Name", { exact: true });

async function search(page: Page, q: string) {
  const card = page.locator(".ct-main");
  await typeInto(card.getByTestId("search-input"), q);
  await expect(card.locator(".grid-wrap")).toHaveAttribute("data-shown-query", q);
}

async function renameFromMenu(page: Page) {
  await gridTab(page).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Rename…" }).click();
  await expect(nameBox(page)).toBeFocused();
}

test("a card's requests name the card and its type", async ({ page }) => {
  // The search itself: the field's `/search/keys` asks for the page, not
  // for a card.
  const search = page.waitForRequest((r) => r.url().includes("/applet/unified_index/search?"));
  await page.goto(GRID);
  const headers = (await search).headers();
  expect(headers["x-datalib-card"]).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab]/);
  expect(headers["x-datalib-card-type"]).toBe("searchView");
});

test("a renamed tab keeps its name through a search and a reload", async ({ page }) => {
  await page.goto(GRID);
  await expect(gridTab(page)).toHaveText(/^Search/);

  // Unrenamed, the grid names its tab after the query.
  await search(page, "kraken");
  await expect(gridTab(page)).toHaveText("Search: kraken");

  await renameFromMenu(page);
  await nameBox(page).fill("Bridge chatter");
  await nameBox(page).press("Enter");
  await expect(gridTab(page)).toHaveText("Bridge chatter");
  await expect(page).toHaveTitle(/^Bridge chatter/);

  // The card no longer names the tab once the person has.
  await search(page, "warp");
  await expect(gridTab(page)).toHaveText("Bridge chatter");

  await page.reload();
  await expect(gridTab(page)).toHaveText("Bridge chatter");
});

test("Escape leaves the tab as it was, and a blank name is refused", async ({ page }) => {
  await page.goto(GRID);
  await expect(gridTab(page)).toHaveText(/^Search/);
  const before = await gridTab(page).innerText();

  await renameFromMenu(page);
  await nameBox(page).fill("Not this");
  await nameBox(page).press("Escape");
  await expect(nameBox(page)).toHaveCount(0);
  await expect(gridTab(page)).toHaveText(before);

  // A double-click on the name is the other way in.
  await gridTab(page).dblclick();
  await nameBox(page).fill("   ");
  await nameBox(page).press("Enter");
  await expect(page.getByText("A name cannot be empty.")).toBeVisible();
  await nameBox(page).press("Escape");
  await expect(gridTab(page)).toHaveText(before);
});
