// The Search card: one query, shown as a list with a preview or as a
// table, whichever was picked last, with source chips that write the query the person could
// have typed.

import { test, expect, type Page } from "@playwright/test";
import { GRID, shownCards, typeInto } from "./grid-helpers";

// In the tab shown: a tab switched away from keeps its cards mounted.
const input = (page: Page) => shownCards(page).getByTestId("search-input");
const viewTab = (page: Page, name: string) =>
  shownCards(page).getByRole("tab", { name, exact: true });
const results = (page: Page) =>
  shownCards(page).getByRole("list", { name: "Results" }).locator(".sc-result");
const listShows = (page: Page) => shownCards(page).locator(".sc-main");
const tableShows = (page: Page) => shownCards(page).locator(".grid-wrap");
const chips = (page: Page) =>
  shownCards(page).getByRole("group", { name: "Sources" }).getByRole("button");

test("the list and the table are two views of one query", async ({ page }) => {
  // As a browser that has never picked a view: the suite's own pick of
  // the table (playwright.config.ts) is taken out once, not on each load.
  await page.addInitScript(() => {
    if (sessionStorage.getItem("view-pick-cleared")) return;
    localStorage.removeItem("datalib-search-view");
    sessionStorage.setItem("view-pick-cleared", "1");
  });
  await page.goto(GRID);
  await expect(viewTab(page, "List and preview")).toHaveAttribute("aria-selected", "true");
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });
  // The table asks nothing until it is shown.
  await expect(shownCards(page).locator(".grid-box .slickgrid-container")).toHaveCount(0);

  await typeInto(input(page), "is:document -kind:nothing");
  await expect(listShows(page)).toHaveAttribute("data-shown-query", "is:document -kind:nothing");

  await viewTab(page, "Table").click();
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", "is:document -kind:nothing");
  await expect(shownCards(page).locator(".grid-box .slickgrid-container")).toBeVisible();
  await expect(results(page).first()).toBeHidden();
  await expect(input(page)).toHaveAttribute("data-query", "is:document -kind:nothing");

  // The view is kept with the card.
  await page.reload();
  await expect(viewTab(page, "Table")).toHaveAttribute("aria-selected", "true");
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", "is:document -kind:nothing");

  await viewTab(page, "List and preview").click();
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });
  await expect(shownCards(page).locator(".grid-box .slickgrid-container")).toBeHidden();
});

test("a new search opens on the view picked last", async ({ page }) => {
  // The suite's pick is the table.
  await page.goto(GRID);
  await expect(viewTab(page, "Table")).toHaveAttribute("aria-selected", "true");
  await viewTab(page, "List and preview").click();
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });

  await page.goto(`${GRID}::q%3D`);
  await expect(viewTab(page, "List and preview")).toHaveAttribute("aria-selected", "true");
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });
});

test("gridView is the grid over a table it is given, and says so when given none", async ({
  page,
}) => {
  await page.goto(`/${encodeURIComponent('gridView({"url":"/applet/unified_index/problems"})')}`);
  await expect(shownCards(page).locator(".grid-wrap")).toHaveAttribute("data-shown-query", "");
  await expect(shownCards(page).getByRole("tablist", { name: "View" })).toHaveCount(0);

  await page.goto("/gridView()");
  await expect(shownCards(page)).toContainText("gridView needs the url of the table to show");
});

test("a source chip writes the source filter, and a typed one lights the chip", async ({
  page,
}) => {
  await page.goto(GRID);
  await expect(chips(page).first()).toContainText("All");
  await expect(chips(page).first()).toHaveAttribute("aria-pressed", "true");
  await chips(page).nth(1).click();
  await expect(input(page)).toHaveAttribute("data-query", /^is:document source_id:\S+$/);
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "true");
  const narrowed = (await input(page).getAttribute("data-query")) ?? "";
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", narrowed);

  await chips(page).first().click();
  await expect(input(page)).toHaveAttribute("data-query", "is:document");
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "false");

  await typeInto(input(page), narrowed);
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "true");
});
