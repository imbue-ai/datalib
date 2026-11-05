// What a link on another site can reach. A browser attaches the session
// cookie to a top-level navigation from anywhere, so any page the person
// has open could send them to a route of the server: `GET
// /api/remote_media` then fetched and kept a URL of the page's choosing,
// and `GET /api/packages` started its fetches. A link may open a page of
// the app and nothing else, and only the browser's own `Sec-Fetch-Site`
// says which requests those are, so this runs in WebKit too, the desktop
// app's engine.

import { test, expect, type Page, type Response } from "@playwright/test";

declare const process: { env: Record<string, string | undefined> };

// Following a link, the browser has only its cookie.
test.use({ extraHTTPHeaders: {} });

/** Opens `origin` as a page holding one link to `path` on this server, and
 *  clicks it. The page is made up here: nothing listens at `origin`. */
async function clickLinkOn(
  page: Page,
  origin: string,
  server: string,
  path: string,
): Promise<Response> {
  const href = new URL(path, server).href;
  await page.route(`${origin}/`, (route) =>
    route.fulfill({
      contentType: "text/html",
      body: `<a id="go" href="${href.replace(/&/g, "&amp;")}">Claim your prize</a>`,
    }),
  );
  await page.goto(`${origin}/`);
  // While a route is in place Chromium does not report `Sec-Fetch-*` to Playwright.
  await page.unrouteAll();
  const answer = page.waitForResponse((r) => r.url() === href);
  await page.locator("#go").click();
  return answer;
}

for (const [from, otherOrigin, site] of [
  ["a web page", () => "http://holosuite-prizes.example", "cross-site"],
  ["another port of this host", (server: URL) => `http://${server.hostname}:47113`, "same-site"],
] as const) {
  test.describe(`a link on ${from}`, () => {
    test.beforeEach(async ({ page, baseURL }) => {
      const signedIn = await page.goto(`${baseURL}/?token=${process.env.DATALIB_TOKEN}`);
      expect(signedIn?.status()).toBe(200);
    });

    // Not `/api/remote_media`: with no allow row on this shared root it refuses
    // whoever asks, so it would pass with no gate at all. `http_tests` has a row.
    for (const path of [
      "/api/packages",
      "/api/config",
      "/applet/unified_document_index/search?q=",
    ]) {
      test(`reaches no route: ${path.split("?")[0]}`, async ({ page, baseURL }) => {
        const server = new URL(baseURL!);
        const answer = await clickLinkOn(page, otherOrigin(server), baseURL!, path);
        const headers = await answer.request().allHeaders();
        expect(headers["sec-fetch-site"]).toBe(site);
        expect(headers["sec-fetch-mode"]).toBe("navigate");
        expect(answer.status()).toBe(403);
      });
    }

    // The control for the refusals: the same click, with the same cookie,
    // does open the app. Without the cookie this would be a 401.
    test("opens a page of the app", async ({ page, baseURL }) => {
      const server = new URL(baseURL!);
      const answer = await clickLinkOn(page, otherOrigin(server), baseURL!, "/data_sources");
      expect((await answer.request().allHeaders())["sec-fetch-site"]).toBe(site);
      expect(answer.status()).toBe(200);
      await expect(page.locator("#app > *").first()).toBeVisible();
    });
  });
}

// A URL typed in the address bar comes from no page, and neither does
// "Open in new tab" on a link of the app's. Opening the API that way is how
// a person or an agent looks at it.
test("the API still opens from the address bar", async ({ page, baseURL }) => {
  await page.goto(`${baseURL}/?token=${process.env.DATALIB_TOKEN}`);
  const answer = await page.goto("/api/health");
  expect((await answer!.request().allHeaders())["sec-fetch-site"]).toBe("none");
  expect(answer!.status()).toBe(200);
});
