// The real browser side of the API token gate (issue #138). Every other
// spec in this suite authenticates through `use.extraHTTPHeaders`, which
// is convenient but is *not* how a user's browser gets in — so this
// spec drops that header and walks the actual path: a launcher opens
// `<url>?token=…` once, the server trades it for a session cookie and
// redirects to the clean URL, and everything the page fetches from then
// on (search, assets, the SSE stream) rides that cookie.

import { test, expect } from "@playwright/test";
import { cardOf, GRID } from "./grid-helpers";

// The same value playwright.config.ts minted and handed to the backend
// via DATALIB_TOKEN. Read from env because the config caches it there
// for worker subprocesses.
declare const process: { env: Record<string, string | undefined> };

const TOKEN = process.env.DATALIB_TOKEN;

// No ambient Authorization header: this file is about the cookie.
test.use({ extraHTTPHeaders: {} });

test("no token means no app, on the page and on the API", async ({ page, request }) => {
  const resp = await page.goto(GRID);
  expect(resp?.status()).toBe(401);
  await expect(page.getByText("This browser isn't authenticated")).toBeVisible();

  const api = await request.get("/applet/unified_document_index/search?q=&limit=1");
  expect(api.status()).toBe(401);
});

test("?token= mints a session cookie, then the app runs on it", async ({ page, context }) => {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();

  const resp = await page.goto(`/?token=${TOKEN}`);
  expect(resp?.status()).toBe(200);
  // Redirected clean: the token must not survive in the address bar,
  // and so not in history or a Referer either.
  expect(new URL(page.url()).search).toBe("");

  const cookie = (await context.cookies()).find((c) => c.name.startsWith("datalib_token_"));
  expect(cookie, "the load should have minted a session cookie").toBeTruthy();
  expect(cookie?.httpOnly).toBe(true);
  expect(cookie?.sameSite).toBe("Lax");

  // The app itself came up, which means the cookie carried the bundle,
  // the /applet/unified_document_index/search behind the Dashboard's latest activity, and
  // everything else the page asked for — no per-request token plumbing
  // anywhere in the UI.
  const firstRow = cardOf(page, "latestActivityView()").locator(".recent").first();
  await expect(firstRow).toBeVisible({ timeout: 20_000 });

  // And a fresh navigation with no token at all now works, because the
  // cookie is in the jar.
  const again = await page.goto(GRID);
  expect(again?.status()).toBe(200);
});

/// An agent restarted datalib under an open tab, and the new server
/// refused the tab's old token. The tab reopened its stream the moment
/// each refusal came back, refetched every card each time and toasted
/// every failure: 35,000 requests in 25 seconds, and a column of toasts.
test("a tab the server no longer knows goes quiet and says why", async ({ page }) => {
  let streams = 0;
  await page.route("**/api/**", (route) => {
    if (new URL(route.request().url()).pathname === "/api/sync/stream") streams += 1;
    return route.fulfill({ status: 401, body: "unauthorized\n" });
  });

  await page.goto(`/?token=${TOKEN}`);

  await expect(page.locator(".datalib-toast")).toHaveText([
    /restarted, so this tab can no longer reach it/,
  ]);
  expect(streams).toBeLessThanOrEqual(1);
});
