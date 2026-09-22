// Remote images are blocked by default (issue #648). The fixture's
// marketing email carries a hero image on one host and a tracking
// pixel on another; opening it must reach neither. What the person
// sees instead is a placeholder per image naming its host, a banner
// counting them, and three ways to load: one placeholder, one host's
// worth, or all — every one of which goes through `/api/remote` on
// this origin, never to the remote host from the page.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, selectRowByUuid } from "./grid-helpers";

declare global {
  interface Window {
    __cspViolations: string[];
  }
}

const HERO = "https://risa.tourism/images/temtibi-lagoon.jpg";
const PIXEL = "https://pixel.ferengi-marketing.example/open.gif?m=risa-promo-001";

// Declared rather than imported, as node-fs.d.ts does for `node:fs`:
// tsconfig's `types` is deliberately narrow.
declare const Buffer: { from(data: string, encoding: "base64"): Uint8Array };

// A 1×1 transparent PNG, for the proxy stand-in below.
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64",
);

function isRemoteHost(url: string): boolean {
  try {
    const u = new URL(url);
    return (
      /^https?:$/.test(u.protocol) && !["localhost", "127.0.0.1", "[::1]"].includes(u.hostname)
    );
  } catch {
    return false;
  }
}

/// Every request the page makes to a host other than this origin, and
/// every policy violation, from now on.
function watch(page: Page): { remote: string[]; violations: () => Promise<string[]> } {
  const remote: string[] = [];
  page.on("request", (req) => {
    if (isRemoteHost(req.url())) remote.push(req.url());
  });
  return {
    remote,
    violations: () => page.evaluate(() => window.__cspViolations ?? []),
  };
}

async function openMarketingEmail(page: Page) {
  const resp = await page.request.get("/applet/unified_index/search?q=&limit=1000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { uuid: string; kind: string; conversation_name: string }[];
  };
  const email = rows.find(
    (r) => r.kind === "Email" && r.conversation_name === "Your shore leave awaits!",
  );
  expect(email, "the fixture's Risa marketing email").toBeDefined();
  await page.goto("/");
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await selectRowByUuid(page, email!.uuid);
  await expect(page.locator(".chat-preview .chat-body")).toBeVisible();
}

test.beforeEach(async ({ context }) => {
  await context.addInitScript(() => {
    window.__cspViolations = [];
    document.addEventListener("securitypolicyviolation", (e) => {
      window.__cspViolations.push(`${e.violatedDirective}: ${e.blockedURI || "inline"}`);
    });
  });
});

test("opening an email with remote images reaches no remote host", async ({ page }) => {
  const seen = watch(page);
  await openMarketingEmail(page);

  const chips = page.locator(".chat-preview button.remote-media");
  await expect(chips).toHaveCount(2);
  await expect(chips.nth(0).locator(".remote-media-host")).toHaveText("risa.tourism");
  await expect(chips.nth(0).locator(".remote-media-alt")).toHaveText("Temtibi Lagoon at sunset");
  await expect(chips.nth(0)).toHaveAttribute("title", new RegExp(HERO.replace(/[.?]/g, "\\$&")));
  await expect(chips.nth(1).locator(".remote-media-host")).toHaveText(
    "pixel.ferengi-marketing.example",
  );

  const banner = page.locator(".chat-preview .remote-banner");
  await expect(banner).toContainText("2 remote images not loaded");
  await expect(banner.locator("button.remote-host")).toHaveText([
    "pixel.ferengi-marketing.example",
    "risa.tourism",
  ]);
  await expect(banner.getByRole("button", { name: "Load all" })).toBeVisible();
  await expect(banner.getByRole("button", { name: "Always load for tng_email" })).toBeVisible();

  // No `<img>` in the document points anywhere but this origin.
  const srcs = await page
    .locator(".chat-preview img")
    .evaluateAll((imgs) => imgs.map((i) => i.getAttribute("src") ?? ""));
  expect(srcs.filter(isRemoteHost)).toEqual([]);
  expect(seen.remote).toEqual([]);
  expect(await seen.violations()).toEqual([]);

  // The second layer is live: an image the sanitizer let through would
  // be refused by the page's policy, with no request made.
  const directive = await page.evaluate(async () => {
    const seen = new Promise<string>((resolve) =>
      document.addEventListener("securitypolicyviolation", (e) => resolve(e.violatedDirective), {
        once: true,
      }),
    );
    const img = document.createElement("img");
    img.src = "https://example.invalid/leak.png";
    document.body.appendChild(img);
    return Promise.race([
      seen,
      new Promise<string>((r) => setTimeout(() => r("no violation"), 3_000)),
    ]);
  });
  // (Playwright still reports the refused attempt as a request, so the
  // violation is the witness here, not the request log.)
  expect(directive).toMatch(/^img-src/);
});

test("images load one, by host, or all — through the proxy only", async ({ page }) => {
  const seen = watch(page);
  // The proxy's answer, stood in for here: what the server does with
  // the URL is the Rust suite's business (http/tests/remote_media.rs),
  // and the fixture's hosts resolve to nothing.
  const proxied: string[] = [];
  await page.route("**/api/remote?**", async (route) => {
    proxied.push(new URL(route.request().url()).searchParams.get("url") ?? "");
    await route.fulfill({ status: 200, contentType: "image/png", body: PNG });
  });
  await openMarketingEmail(page);

  const chips = page.locator(".chat-preview button.remote-media");
  const banner = page.locator(".chat-preview .remote-banner");
  const loadedImages = page.locator('.chat-preview img[src^="/api/remote?url="]');

  // One placeholder: its image only.
  await chips.nth(1).click();
  await expect(chips).toHaveCount(1);
  await expect(loadedImages).toHaveCount(1);
  await expect(banner).toContainText("1 remote image not loaded");
  expect(proxied).toEqual([PIXEL]);

  // A host's worth, from the banner.
  await banner.locator("button.remote-host", { hasText: "risa.tourism" }).click();
  await expect(chips).toHaveCount(0);
  await expect(loadedImages).toHaveCount(2);
  await expect(banner).toContainText("Remote images loaded.");
  await expect(banner.getByRole("button", { name: "Always load for tng_email" })).toBeVisible();
  expect(proxied).toEqual([PIXEL, HERO]);

  // And the bytes came back through this origin and rendered.
  const hero = page.locator(`.chat-preview img[alt="Temtibi Lagoon at sunset"]`);
  await expect.poll(() => hero.evaluate((i) => (i as HTMLImageElement).naturalWidth)).toBe(1);

  expect(seen.remote).toEqual([]);
  expect(await seen.violations()).toEqual([]);
});

test("Load all puts every image back at once", async ({ page }) => {
  const seen = watch(page);
  await page.route("**/api/remote?**", (route) =>
    route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
  );
  await openMarketingEmail(page);
  await page
    .locator(".chat-preview .remote-banner")
    .getByRole("button", { name: "Load all" })
    .click();
  await expect(page.locator(".chat-preview button.remote-media")).toHaveCount(0);
  await expect(page.locator('.chat-preview img[src^="/api/remote?url="]')).toHaveCount(2);
  expect(seen.remote).toEqual([]);
  expect(await seen.violations()).toEqual([]);
});
