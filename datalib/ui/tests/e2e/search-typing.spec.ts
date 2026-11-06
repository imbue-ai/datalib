// Typing in the search bar asks the server only for what the person
// stops on: keys typed at speed are one search, a space before the next
// word is none, and a search typed past is given up, every tab of it.
// The server stops ranking one given up (the gateway hangs up on the
// applet, `datalib-http`'s `applets::Hangup`).

import { test, expect, type Page, type Request } from "@playwright/test";
import { EVERY_ROW, SEARCH_ROWS, shownCards, SEARCH_SETTLE } from "./grid-helpers";

const field = (page: Page) => page.getByTestId("search-input");
const shownQuery = (page: Page) => page.locator(".grid-wrap");

/// The query each request to the search applet asked, as it went out.
function asked(request: Request): string | null {
  const url = new URL(request.url());
  if (!url.pathname.startsWith("/applet/unified_document_index/search")) return null;
  return url.searchParams.get("q");
}

/// Every search request from here on, and what became of each.
function record(page: Page) {
  const sent: { q: string; request: Request }[] = [];
  const failed = new Map<Request, string>();
  page.on("request", (r) => {
    const q = asked(r);
    if (q !== null) sent.push({ q, request: r });
  });
  page.on("requestfailed", (r) => failed.set(r, r.failure()?.errorText ?? ""));
  return { sent, failed };
}

async function opened(page: Page) {
  await page.goto(EVERY_ROW);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await field(page).click();
}

test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

test("typing asks nothing, and Enter asks for the search once", async ({ page }) => {
  await opened(page);
  const { sent } = record(page);
  await field(page).pressSequentially("warp core", { delay: 60 });
  expect(sent, "nothing went out while typing").toEqual([]);
  await field(page).press("Enter");
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core", {
    timeout: SEARCH_SETTLE,
  });
  expect(sent.filter((s) => s.q === "warp core").length).toBe(1);
});

/// A half-typed key once ran as a search and replaced the results while
/// its suggestions were still open.
test("a half-typed key is not searched for", async ({ page }) => {
  await opened(page);
  const shown = await shownQuery(page).getAttribute("data-shown-query");
  await field(page).pressSequentially("from:", { delay: 60 });
  await expect(page.locator(".cm-tooltip-autocomplete")).toBeVisible();
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", shown ?? "");
});

test("Enter on what is already shown asks nothing new", async ({ page }) => {
  await opened(page);
  await field(page).pressSequentially("warp core", { delay: 60 });
  await field(page).press("Enter");
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core", {
    timeout: SEARCH_SETTLE,
  });
  const { sent } = record(page);
  await field(page).click();
  await field(page).press("End");
  await page.keyboard.insertText(" ");
  await field(page).press("Enter");
  // The search with words after it is the one that shows nothing moved.
  await field(page).click();
  await page.keyboard.insertText("b");
  await field(page).press("Enter");
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core b", {
    timeout: SEARCH_SETTLE,
  });
  expect(new Set(sent.map((s) => s.q))).toEqual(new Set(["warp core b"]));
});

test("a search replaced is given up, every request of it", async ({ page }) => {
  await opened(page);
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/applet/unified_document_index/search**", async (route) => {
    if (asked(route.request()) === "warp") await held;
    await route.fallback().catch(() => {});
  });
  const { sent, failed } = record(page);

  await field(page).pressSequentially("warp", { delay: 60 });
  await field(page).press("Enter");
  const first = () => sent.filter((s) => s.q === "warp").map((s) => s.request);
  await expect
    .poll(() => first().length, { message: "the first search never went out" })
    .toBeGreaterThanOrEqual(1);
  await field(page).click();
  await field(page).press("End");
  await field(page).pressSequentially(" core", { delay: 60 });
  await field(page).press("Enter");
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core", {
    timeout: SEARCH_SETTLE,
  });

  await expect
    .poll(() => first().filter((r) => !failed.has(r)).length, {
      message: "a request of the search typed past was left running",
    })
    .toBe(0);
  for (const r of first()) expect(failed.get(r)).toMatch(/ABORTED|cancel/i);
  release();
});
