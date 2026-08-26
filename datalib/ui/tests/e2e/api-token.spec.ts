// The real browser side of the API token gate (issue #138). Every other
// spec in this suite authenticates through `use.extraHTTPHeaders`, which
// is convenient but is *not* how a user's browser gets in — so this
// spec drops that header and walks the actual path: a launcher opens
// `<url>?token=…` once, the server trades it for a session cookie and
// redirects to the clean URL, and everything the page fetches from then
// on (search, assets, the SSE stream) rides that cookie.
//
// Worth having as an e2e rather than only a Rust integration test: the
// properties under test are the browser's (does it keep the cookie
// across a redirect? does it attach it to subresource requests?), and
// those are exactly what a `oneshot` router test can't observe.

import { test, expect } from "@playwright/test";

// The same value playwright.config.ts minted and handed to the backend
// via DATALIB_TOKEN. Read from env because the config caches it there
// for worker subprocesses.
//
// Declared locally rather than pulling in @types/node: tsconfig.json's
// `types` is deliberately narrow (just vitest/globals), and specs are
// inside its `include` globs — playwright.config.ts gets away with bare
// `process` only because it sits outside them. One line beats a
// dependency and a global type widening.
declare const process: { env: Record<string, string | undefined> };

const TOKEN = process.env.DATALIB_TOKEN;

// No ambient Authorization header: this file is about the cookie.
test.use({ extraHTTPHeaders: {} });

test("no token means no app, on the page and on the API", async ({
  page,
  request,
}) => {
  const resp = await page.goto("/");
  expect(resp?.status()).toBe(401);
  await expect(page.getByText("This browser isn't authenticated")).toBeVisible();

  const api = await request.get("/applet/unified_index/search?q=&limit=1");
  expect(api.status()).toBe(401);
});

test("?token= mints a session cookie, then the app runs on it", async ({
  page,
  context,
}) => {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();

  const resp = await page.goto(`/?token=${TOKEN}`);
  expect(resp?.status()).toBe(200);
  // Redirected clean: the token must not survive in the address bar,
  // and so not in history or a Referer either.
  expect(new URL(page.url()).search).toBe("");

  const cookie = (await context.cookies()).find((c) =>
    c.name.startsWith("datalib_token_"),
  );
  expect(cookie, "the load should have minted a session cookie").toBeTruthy();
  expect(cookie?.httpOnly).toBe(true);
  expect(cookie?.sameSite).toBe("Lax");

  // The app itself came up, which means the cookie carried the bundle,
  // the /applet/unified_index/search behind the grid, and everything else the page asked
  // for — no per-request token plumbing anywhere in the UI.
  const firstRow = page.locator('.ag-center-cols-container [role="row"]').first();
  await expect(firstRow).toBeVisible({ timeout: 20_000 });

  // And a fresh navigation with no token at all now works, because the
  // cookie is in the jar.
  const again = await page.goto("/");
  expect(again?.status()).toBe(200);
});
