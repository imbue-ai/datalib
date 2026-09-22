// The per-source switch (issue #648): "Always load for <source>" in the
// document view writes `load_remote_images = true` onto the source's
// `[[groups]]` entry, after which its documents open with their remote
// images already loaded — through the proxy — and "Stop" takes the
// line out again. Writes config.toml, so it runs on its own root.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, selectRowByUuid } from "./grid-helpers";

declare const Buffer: { from(data: string, encoding: "base64"): Uint8Array };
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64",
);

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

async function configText(page: Page): Promise<string> {
  const resp = await page.request.get("/api/config");
  expect(resp.ok()).toBeTruthy();
  return ((await resp.json()) as { text: string }).text;
}

test("the per-source setting persists and loads on open", async ({ page }) => {
  const remote: string[] = [];
  page.on("request", (req) => {
    const host = new URL(req.url()).hostname;
    if (/^https?:/.test(req.url()) && !["localhost", "127.0.0.1"].includes(host)) {
      remote.push(req.url());
    }
  });
  await page.route("**/api/remote?**", (route) =>
    route.fulfill({ status: 200, contentType: "image/png", body: PNG }),
  );
  await openMarketingEmail(page);

  const banner = page.locator(".chat-preview .remote-banner");
  const chips = page.locator(".chat-preview button.remote-media");
  const loadedImages = page.locator('.chat-preview img[src^="/api/remote?url="]');
  await expect(chips).toHaveCount(2);
  expect(await configText(page)).not.toContain("load_remote_images");

  await banner.getByRole("button", { name: "Always load for tng_email" }).click();
  await expect(banner).toContainText("loaded automatically for tng_email");
  await expect(chips).toHaveCount(0);
  await expect(loadedImages).toHaveCount(2);
  expect(await configText(page)).toMatch(/id = "tng_email"\nload_remote_images = true\n/);

  // A fresh page reads the setting from the applet and renders the
  // images proxied from the start: no placeholder is ever drawn.
  await openMarketingEmail(page);
  await expect(loadedImages).toHaveCount(2);
  await expect(chips).toHaveCount(0);
  await expect(banner).toContainText("loaded automatically for tng_email");

  await banner.getByRole("button", { name: "Stop" }).click();
  await expect(banner.getByRole("button", { name: "Always load for tng_email" })).toBeVisible();
  expect(await configText(page)).not.toContain("load_remote_images");

  await openMarketingEmail(page);
  await expect(chips).toHaveCount(2);
  expect(remote).toEqual([]);
});
