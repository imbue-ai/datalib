// Remote images are blocked (issue #648). The fixture's marketing email
// carries a hero image on one host and a tracking pixel on another;
// opening it must reach neither. What the person sees instead is a
// placeholder per image naming its host and a banner counting them.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, selectRowByUuid } from "./grid-helpers";

declare global {
  interface Window {
    __cspViolations: string[];
  }
}

const HERO = "https://risa.tourism/images/temtibi-lagoon.jpg";

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

test("opening an email with remote images reaches no remote host", async ({ page, context }) => {
  await context.addInitScript(() => {
    window.__cspViolations = [];
    document.addEventListener("securitypolicyviolation", (e) => {
      window.__cspViolations.push(`${e.violatedDirective}: ${e.blockedURI || "inline"}`);
    });
  });
  const remoteRequests: string[] = [];
  page.on("request", (req) => {
    if (isRemoteHost(req.url())) remoteRequests.push(req.url());
  });
  await openMarketingEmail(page);

  const chips = page.locator(".chat-preview .remote-media");
  await expect(chips).toHaveCount(2);
  await expect(chips.nth(0).locator(".remote-media-host")).toHaveText("risa.tourism");
  await expect(chips.nth(0).locator(".remote-media-alt")).toHaveText("Temtibi Lagoon at sunset");
  await expect(chips.nth(0)).toHaveAttribute(
    "title",
    new RegExp(`^${HERO.replace(/[.?]/g, "\\$&")}`),
  );
  await expect(chips.nth(1).locator(".remote-media-host")).toHaveText(
    "pixel.ferengi-marketing.example",
  );

  const banner = page.locator(".chat-preview .remote-banner");
  await expect(banner).toContainText("2 remote images not loaded");
  await expect(banner.locator(".remote-host")).toHaveText([
    "pixel.ferengi-marketing.example",
    "risa.tourism",
  ]);

  // No `<img>` in the document points anywhere but this origin, no
  // request left for one, and the policy had nothing to refuse.
  const srcs = await page
    .locator(".chat-preview img")
    .evaluateAll((imgs) => imgs.map((i) => i.getAttribute("src") ?? ""));
  expect(srcs.filter(isRemoteHost)).toEqual([]);
  expect(remoteRequests).toEqual([]);
  expect(await page.evaluate(() => window.__cspViolations)).toEqual([]);

  // The second layer is live: an image the sanitizer let through would
  // be refused by the page's policy. (Playwright still reports the
  // refused attempt as a request, so the violation is the witness.)
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
  expect(directive).toMatch(/^img-src/);
});
