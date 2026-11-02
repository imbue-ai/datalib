// Free text is answered three ways, a tab each, and merged in a fourth,
// Best, which opens as the search starts. The others say when they are
// ready, and nothing on screen moves until the reader picks one.

import { test, expect, type Page, type Route } from "@playwright/test";
import {
  EVERY_ROW,
  GRID,
  SEARCH_ROWS,
  searchAndSettle,
  typeInto,
  type GridApi,
} from "./grid-helpers";

type Tab = "best" | "fields" | "words" | "meaning";

const tabButton = (page: Page, tab: Tab) => page.locator(`.answer-tabs [data-tab="${tab}"]`);
const firstUuid = (page: Page) =>
  page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.uuidAt(0),
  ) as Promise<string | null>;
const tabOf = (route: Route) =>
  (new URL(route.request().url()).searchParams.get("tab") ?? "best") as Tab;

/// Hold back every answer for `tabs` until the returned release is called.
async function hold(page: Page, tabs: Tab[]): Promise<() => void> {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/applet/unified_document_index/search?**", async (route) => {
    if (tabs.includes(tabOf(route))) await gate;
    await route.fallback();
  });
  return release;
}

test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

test("the best answer opens, and the others say when they are ready", async ({ page }) => {
  const release = await hold(page, ["words", "meaning"]);
  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });

  await searchAndSettle(page, "enterprise");
  await expect(page.locator(".grid-wrap")).toHaveAttribute("data-shown-tab", "best");
  await expect(tabButton(page, "best")).toHaveClass(/is-on/);
  await expect(tabButton(page, "words")).toHaveClass(/is-pending/);
  await expect(tabButton(page, "meaning")).toHaveClass(/is-pending/);
  const shown = await firstUuid(page);
  expect(shown).not.toBeNull();

  release();
  await expect(tabButton(page, "words")).toHaveClass(/is-new/, { timeout: 90_000 });
  await expect(tabButton(page, "meaning")).not.toHaveClass(/is-pending/, { timeout: 90_000 });
  // Their answers arriving moved nothing.
  await expect(page.locator(".grid-wrap")).toHaveAttribute("data-shown-tab", "best");
  expect(await firstUuid(page)).toBe(shown);

  await tabButton(page, "words").click();
  await expect(page.locator(".grid-wrap")).toHaveAttribute("data-shown-tab", "words");
  await expect(tabButton(page, "words")).toHaveClass(/is-on/);
  await expect(tabButton(page, "words")).not.toHaveClass(/is-new/);
});

test("when the best answer fails, the first tab with rows opens", async ({ page }) => {
  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await page.route("**/applet/unified_document_index/search?**", async (route) => {
    if (tabOf(route) !== "best") return route.fallback();
    await route.fulfill({ status: 500, json: { error: "held back by the test" } });
  });

  // Best's own rows fail too, so the search never settles on it.
  await typeInto(page.getByTestId("search-input"), "enterprise");
  await expect(page.locator(".grid-wrap")).toHaveAttribute(
    "data-shown-tab",
    /fields|words|meaning/,
    { timeout: 90_000 },
  );
  await expect(tabButton(page, "best")).toHaveClass(/is-failed/);
});

/// The source chips wrapped, so a count that grew, or a tab that came in,
/// added a row and moved every result under it, often just as it was
/// about to be clicked.
test("the chips row keeps its height whatever the counts", async ({ page }) => {
  await page.setViewportSize({ width: 760, height: 800 });
  await page.goto(GRID);
  const row = page.locator(".view-row");
  await searchAndSettle(page, "is:document");
  const height = (await row.boundingBox())!.height;
  await searchAndSettle(page, "picard");
  await expect(tabButton(page, "words")).toBeVisible();
  expect((await row.boundingBox())!.height).toBe(height);
  await searchAndSettle(page, "");
  expect((await row.boundingBox())!.height).toBe(height);
});

/// The chips are one line that scrolls, so a typed `source:` lit a chip
/// out of sight and the row seemed to say "All".
test("a source picked by typing is scrolled into view", async ({ page }) => {
  await page.setViewportSize({ width: 760, height: 800 });
  await page.goto(GRID);
  const chips = page.locator(".source-chips .source-chip");
  await expect(chips.nth(2)).toBeVisible();
  const id = await chips.last().getAttribute("data-source");
  await searchAndSettle(page, `source:${id}`);
  await expect(page.locator(".source-chips .source-chip.is-on")).toBeInViewport();
});
