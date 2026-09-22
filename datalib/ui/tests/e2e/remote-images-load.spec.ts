// Loading a document's remote images (issue #648, second half): a
// click records an allow row — one URL, one host, this document, this
// source — the body re-renders with those references proxied through
// `/api/remote_media`, the rows survive a reload and can be forgotten
// from the banner. Writes the allow store, so it runs on its own root.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, selectRowByUuid } from "./grid-helpers";

declare const Buffer: { from(data: string, encoding: "base64"): Uint8Array };
// A 1×1 transparent PNG, standing in for the server's answer: the
// fixture's hosts resolve to nothing, and what the server does with a
// URL is the Rust suite's business (http/tests/remote_media.rs).
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64",
);
const HERO = "https://risa.tourism/images/temtibi-lagoon.jpg";
const PIXEL = "https://pixel.ferengi-marketing.example/open.gif?m=risa-promo-001";

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

async function allowRows(page: Page): Promise<{ scope: string; key: string }[]> {
  const resp = await page.request.get("/api/remote_media/allow");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { scope: string; key: string }[] };
  return rows.map(({ scope, key }) => ({ scope, key }));
}

test("a click records an allow, which loads, persists and can be forgotten", async ({ page }) => {
  const remoteRequests: string[] = [];
  page.on("request", (req) => {
    if (isRemoteHost(req.url())) remoteRequests.push(req.url());
  });
  const proxied: string[] = [];
  await page.route("**/api/remote_media?**", async (route) => {
    proxied.push(new URL(route.request().url()).searchParams.get("url") ?? "");
    await route.fulfill({ status: 200, contentType: "image/png", body: PNG });
  });
  await openMarketingEmail(page);

  const banner = page.locator(".chat-preview .remote-banner");
  const chips = page.locator(".chat-preview button.remote-media");
  const loaded = page.locator('.chat-preview img[src^="/api/remote_media?url="]');
  const rules = banner.locator(".remote-rule");
  await expect(chips).toHaveCount(2);
  expect(await allowRows(page)).toEqual([]);

  // One placeholder → a `url` row; only that image loads.
  await chips.nth(1).click();
  await expect(chips).toHaveCount(1);
  await expect(loaded).toHaveCount(1);
  await expect(banner).toContainText("1 remote image not loaded");
  await expect(rules).toHaveText([new RegExp(PIXEL.replace(/[.?]/g, "\\$&"))]);
  expect(await allowRows(page)).toEqual([{ scope: "url", key: PIXEL }]);
  expect(proxied).toEqual([PIXEL]);

  // A host chip → a `host` row; the rest loads, and the banner names
  // both rules.
  await banner.locator("button.remote-host", { hasText: "risa.tourism" }).click();
  await expect(chips).toHaveCount(0);
  await expect(loaded).toHaveCount(2);
  await expect(banner).toContainText("Remote images loaded.");
  await expect(rules).toHaveCount(2);
  await expect(rules.filter({ hasText: "everything on risa.tourism" })).toHaveCount(1);
  expect(proxied).toEqual([PIXEL, HERO]);

  // The bytes came back through this origin and drew something.
  const hero = page.locator(`.chat-preview img[alt="Temtibi Lagoon at sunset"]`);
  await expect.poll(() => hero.evaluate((i) => (i as HTMLImageElement).naturalWidth)).toBe(1);

  // A fresh page reads the rows and renders the images loaded from
  // the start: no placeholder is ever drawn.
  await openMarketingEmail(page);
  await expect(loaded).toHaveCount(2);
  await expect(chips).toHaveCount(0);

  // Forgetting a rule holds its images again.
  await rules.filter({ hasText: "everything on risa.tourism" }).locator("button").click();
  await expect(chips).toHaveCount(1);
  await expect(chips.first().locator(".remote-media-host")).toHaveText("risa.tourism");
  await rules.first().locator("button").click();
  await expect(chips).toHaveCount(2);
  expect(await allowRows(page)).toEqual([]);

  // The source-wide switch, and the document-wide one.
  await banner.getByRole("button", { name: "Always for tng_email" }).click();
  await expect(chips).toHaveCount(0);
  await expect(rules).toHaveText(["everything from tng_email ✕"]);
  expect(await allowRows(page)).toEqual([{ scope: "source", key: "tng_email" }]);
  await rules.first().locator("button").click();
  await expect(chips).toHaveCount(2);

  await banner.getByRole("button", { name: "Load all" }).click();
  await expect(chips).toHaveCount(0);
  await expect(rules).toHaveText(["everything in this document ✕"]);
  const [doc] = await allowRows(page);
  expect(doc.scope).toBe("document");
  await rules.first().locator("button").click();
  await expect(chips).toHaveCount(2);

  // The rules are a table like any other.
  await banner.getByRole("link", { name: "all rules" }).click();
  await expect(page.locator(".tg-grid").last()).toBeVisible();

  expect(remoteRequests).toEqual([]);
});
