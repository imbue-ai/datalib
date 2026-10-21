// The real browser side of the API token gate (issue #138). Every other
// spec in this suite authenticates through `use.extraHTTPHeaders`, which
// is convenient but is *not* how a user's browser gets in — so this
// spec drops that header and walks the actual path: a launcher opens
// `<url>?token=…` once, the page moves the token into `localStorage` and
// out of the address bar, and sends it with everything it asks for
// (search, document assets, the SSE stream). The server sets no cookie.

import { test, expect } from "@playwright/test";
import { cardOf, GRID } from "./grid-helpers";

// The same value playwright.config.ts minted and handed to the backend
// via DATALIB_TOKEN. Read from env because the config caches it there
// for worker subprocesses.
declare const process: { env: Record<string, string | undefined> };

const TOKEN = process.env.DATALIB_TOKEN;

// No ambient Authorization header: this file is about the page's own.
test.use({ extraHTTPHeaders: {} });

test("no token means no app, on the page and on the API", async ({ page, request }) => {
  const resp = await page.goto(GRID);
  // The shell is public; it holds no data, and it is what says why.
  expect(resp?.status()).toBe(200);
  await expect(page.getByText("This browser isn't authenticated")).toBeVisible();

  const api = await request.get("/applet/unified_document_index/search?q=&limit=1");
  expect(api.status()).toBe(401);
});

test("?token= is kept by the page, not the cookie jar, and the app runs on it", async ({
  page,
  context,
}) => {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();

  const streamed = page.waitForRequest((r) => r.url().includes("/api/sync/stream"));
  await page.goto(`/?token=${TOKEN}`);
  // Gone from the address bar, so not in history, a bookmark or a
  // copied link.
  await expect.poll(() => new URL(page.url()).search).toBe("");
  expect(await page.evaluate(() => localStorage.getItem("datalib-api-token"))).toBe(TOKEN);
  expect(await context.cookies()).toEqual([]);

  // The app itself came up: the Dashboard's latest activity is a search
  // through the applet, which needed the header.
  const firstRow = cardOf(page, "latestActivityView()").locator(".recent").first();
  await expect(firstRow).toBeVisible({ timeout: 20_000 });
  // The stream cannot send a header, so it carries the token in its URL.
  expect(new URL((await streamed).url()).searchParams.get("token")).toBe(TOKEN);

  // A fresh navigation with no token in the URL works from storage.
  await page.goto("/");
  await expect(cardOf(page, "latestActivityView()").locator(".recent").first()).toBeVisible({
    timeout: 20_000,
  });
  await expect(page.getByText("This browser isn't authenticated")).toHaveCount(0);
});
