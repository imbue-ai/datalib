// Typing in the search bar asks the server only for what the person
// stops on: keys typed at speed are one search, a space before the next
// word is none, and a search typed past is given up, every tab of it.
// The server stops ranking one given up (the gateway hangs up on the
// applet, `datalib-http`'s `applets::Hangup`).

import { test, expect, type Page, type Request } from "@playwright/test";
import { EVERY_ROW, SEARCH_ROWS, shownCards } from "./grid-helpers";

const field = (page: Page) => shownCards(page).getByTestId("search-input");
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

test("keys typed at speed are one search", async ({ page }) => {
  await opened(page);
  const { sent } = record(page);
  await field(page).pressSequentially("warp core", { delay: 60 });
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core");
  expect(sent.length, "the search went out").toBeGreaterThan(0);
  expect(new Set(sent.map((s) => s.q))).toEqual(new Set(["warp core"]));
});

/// The space before the next word, left long enough to settle, asked
/// for the whole search again: every tab, the page and the source chips.
test("a space before the next word asks nothing new", async ({ page }) => {
  await opened(page);
  await field(page).pressSequentially("warp core", { delay: 60 });
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core");
  const { sent } = record(page);
  await field(page).pressSequentially(" b", { delay: 800 });
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core b");
  expect(new Set(sent.map((s) => s.q))).toEqual(new Set(["warp core b"]));
});

test("a search typed past is given up, every request of it", async ({ page }) => {
  await opened(page);
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route("**/applet/unified_document_index/search**", async (route) => {
    if (asked(route.request()) === "warp") await held;
    await route.fallback().catch(() => {});
  });
  const { sent, failed } = record(page);

  await field(page).pressSequentially("warp", { delay: 60 });
  const first = () => sent.filter((s) => s.q === "warp").map((s) => s.request);
  // The page, the four tabs' counts and the source chips.
  await expect
    .poll(() => first().length, { message: "the first search never went out" })
    .toBeGreaterThanOrEqual(6);
  await field(page).pressSequentially(" core", { delay: 60 });
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp core");

  await expect
    .poll(() => first().filter((r) => !failed.has(r)).length, {
      message: "a request of the search typed past was left running",
    })
    .toBe(0);
  for (const r of first()) expect(failed.get(r)).toMatch(/ABORTED|cancel/i);
  release();
});
