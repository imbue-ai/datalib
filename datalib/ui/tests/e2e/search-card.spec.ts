// The Search card: one query, shown as a list with a preview or as a
// table, whichever was picked last, with source chips that write the query the person could
// have typed.

import { test, expect, type Page } from "@playwright/test";
import { GRID, shownCards, stubStream, typeInto } from "./grid-helpers";

// In the tab shown: a tab switched away from keeps its cards mounted.
const input = (page: Page) => page.getByTestId("search-input");
const viewTab = (page: Page, name: string) =>
  shownCards(page).getByRole("tab", { name, exact: true });
const results = (page: Page) =>
  shownCards(page).getByRole("list", { name: "Results" }).locator(".sc-result");
const listShows = (page: Page) => shownCards(page).locator(".sc-main");
const tableShows = (page: Page) => shownCards(page).locator(".grid-wrap");
const chips = (page: Page) =>
  shownCards(page).getByRole("group", { name: "Sources" }).getByRole("button");
const sortPick = (page: Page) => shownCards(page).getByTestId("search-sort");
const pickedResult = (page: Page) => shownCards(page).locator(".sc-result.is-picked");

const SEARCH_PATH = "/applet/unified_document_index/search";

/// The first document the server lists for `sort`, by the title the list
/// shows for it.
async function firstTitle(page: Page, sort: string): Promise<string> {
  const params = new URLSearchParams({ q: "is:document", limit: "1", sort });
  const resp = await page.request.get(`${SEARCH_PATH}?${params}`);
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { conversation_name: string | null }[] };
  expect(rows[0]?.conversation_name, `${sort} leads with a titled row`).toBeTruthy();
  return rows[0].conversation_name!;
}

/// Does `act`, and waits for the rows it should ask for: named by the
/// order, since a view just shown first catches up on the one before.
async function asksFor(page: Page, sort: string, act: () => Promise<unknown>) {
  await Promise.all([
    page.waitForResponse((r) => {
      const url = new URL(r.url());
      return url.pathname === SEARCH_PATH && url.searchParams.get("sort") === sort && r.ok();
    }),
    act(),
  ]);
}

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
  await page.goto(
    `/${encodeURIComponent('gridView({"url":"/applet/unified_document_index/problems"})')}`,
  );
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
  await expect(input(page)).toHaveAttribute("data-query", /^is:document source:\S+$/);
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "true");
  const narrowed = (await input(page).getAttribute("data-query")) ?? "";
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", narrowed);

  await chips(page).first().click();
  await expect(input(page)).toHaveAttribute("data-query", "is:document");
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "false");

  await typeInto(input(page), narrowed);
  await expect(chips(page).nth(1)).toHaveAttribute("aria-pressed", "true");
});

test("the sort control orders either view, and agrees with the table's headers", async ({
  page,
}) => {
  await page.goto(GRID);
  await viewTab(page, "List and preview").click();
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });
  await expect(sortPick(page).locator("option:checked")).toHaveText("Newest first");

  await asksFor(page, "subject_ref:asc", () =>
    sortPick(page).selectOption({ label: "Title A to Z" }),
  );
  await expect(results(page).first()).toContainText(await firstTitle(page, "subject_ref:asc"));

  // The table shows the same order, marked on its header.
  await viewTab(page, "Table").click();
  const subject = shownCards(page).locator(".slick-header-column", { hasText: "Subject" });
  await expect(subject.locator(".slick-sort-indicator-asc")).toBeVisible();

  // A date's first click is its newest, which the control then names.
  const touched = shownCards(page).locator(".slick-header-column", { hasText: "Touched" });
  await asksFor(page, "touched_at:desc", () => touched.click());
  await expect(sortPick(page).locator("option:checked")).toHaveText("Newest first");
  await expect(subject.locator(".slick-sort-indicator-asc")).toHaveCount(0);

  // Size is a hidden column, with no header to hold the order.
  await asksFor(page, "byte_size:desc", () =>
    sortPick(page).selectOption({ label: "Largest first" }),
  );

  await page.reload();
  await expect(sortPick(page).locator("option:checked")).toHaveText("Largest first");
  await viewTab(page, "List and preview").click();
  await expect(results(page).first()).toContainText(await firstTitle(page, "byte_size:desc"));
  await expect(results(page).first()).toContainText(/\d (B|kB|MB)/);
});

/// A sync used to drop the list back to its first page and move the pick
/// to its first result, whatever the person had scrolled to.
test("the list keeps what is loaded and picked when the index moves", async ({ page }) => {
  const commit = await stubStream(page);
  await page.goto(GRID);
  await viewTab(page, "List and preview").click();
  await expect(results(page)).toHaveCount(50, { timeout: 10_000 });

  await shownCards(page).getByRole("button", { name: "Show more" }).click();
  await expect(results(page)).toHaveCount(100);
  await results(page).nth(70).click();
  const title = await pickedResult(page).locator(".sc-title").innerText();

  const reread = page.waitForResponse((r) => {
    const url = new URL(r.url());
    return url.pathname === SEARCH_PATH && url.searchParams.get("limit") === "100";
  });
  commit();
  await reread;
  await expect(results(page)).toHaveCount(100);
  await expect(pickedResult(page).locator(".sc-title")).toHaveText(title);

  await page.unrouteAll({ behavior: "ignoreErrors" });
});

/// A search is one request for its rows, whichever view shows, and what it
/// could not reach is said above them in both.
test("a search says above its results what it could not reach, in either view", async ({
  page,
}) => {
  const asked: URL[] = [];
  await page.route(`**${SEARCH_PATH}?**`, async (route) => {
    const url = new URL(route.request().url());
    asked.push(url);
    const response = await route.fetch();
    const body = (await response.json()) as { query_echo: Record<string, unknown> };
    if (url.searchParams.get("q")?.startsWith("warp core")) {
      body.query_echo.without_vectors = { total: 1511, by_source: [["slack", 1511]] };
      body.query_echo.partial = "Part of this search could not answer";
    }
    await route.fulfill({ response, json: body });
  });
  await page.goto(GRID);
  await viewTab(page, "List and preview").click();
  await expect(results(page).first()).toBeVisible({ timeout: 10_000 });
  const notices = shownCards(page).getByTestId("search-notice");
  await expect(notices).toHaveCount(0);

  asked.length = 0;
  await typeInto(input(page), "warp core");
  await expect(listShows(page)).toHaveAttribute("data-shown-query", "warp core");
  await expect(notices).toHaveText([
    "Part of this search could not answer",
    "1,511 candidates lack semantic embedding vectors",
  ]);
  await expect(notices.nth(1)).toHaveAttribute("title", /slack 1,511/);
  expect(asked.filter((u) => u.searchParams.get("q") === "warp core").length).toBe(1);
  expect(asked.some((u) => u.searchParams.has("tab"))).toBe(false);

  await viewTab(page, "Table").click();
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", "warp core");
  await expect(notices).toHaveCount(2);

  await typeInto(input(page), "tea semantic:off");
  await expect(tableShows(page)).toHaveAttribute("data-shown-query", "tea semantic:off");
  await expect(notices).toHaveCount(0);

  await page.unrouteAll({ behavior: "ignoreErrors" });
});
