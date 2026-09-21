import { test, expect } from "@playwright/test";

// The toolbar across the top is the way home: "Data sources" reveals
// the sources card — opening one only when none is showing — and
// "New card" is the "+" strip's gesture from the top. The status bar's
// "Logs" reveals the log the same way. All act on the URL-synced
// miller stack, so the path says what they did.

async function stackPath(page: import("@playwright/test").Page): Promise<string> {
  return decodeURIComponent(await page.evaluate(() => location.pathname));
}

test.describe("toolbar", () => {
  test("Data sources opens the sources card once, then only reveals it", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".miller-col")).toHaveCount(1);

    await page.getByRole("button", { name: "Data sources" }).click();
    await expect(page.locator(".miller-col")).toHaveCount(2);
    await expect(page.locator(".m2-head")).toBeVisible({ timeout: 10_000 });
    expect(await stackPath(page)).toContain("sourcesView()");

    // A second press finds the card already there.
    await page.getByRole("button", { name: "Data sources" }).click();
    await expect(page.locator(".miller-col")).toHaveCount(2);
  });

  test("New card appends a gallery column", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "New card" }).click();
    await expect(page.locator(".gv-row").first()).toBeVisible({ timeout: 10_000 });
    expect(await stackPath(page)).toContain("galleryView()");
  });

  test("the status bar's Logs opens the log over every run, once", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Logs" }).click();
    const col = page.locator(".miller-col").filter({ has: page.locator(".rl-panel") });
    await expect(col).toBeVisible({ timeout: 10_000 });
    await expect(col.locator(".miller-col-title")).toHaveText("Log · everything");
    expect(await stackPath(page)).toContain("logView()");

    await page.getByRole("button", { name: "Logs" }).click();
    await expect(page.locator(".miller-col")).toHaveCount(2);
  });

  test("the syncing pill is absent when nothing runs", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".datalib-toolbar")).toBeVisible();
    await expect(page.locator(".sync-indicator")).toHaveCount(0);
  });
});
