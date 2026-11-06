// The DACTAL card with no query lists the datasets a query can start
// from, as DACTAL's own query page does, rather than running one for you.

import { test, expect } from "@playwright/test";

declare const process: { env: Record<string, string | undefined> };
const TOKEN = process.env.DATALIB_TOKEN;

// As in dactal-sandbox.spec.ts: the suite's ambient `Authorization`
// header would make the sandboxed frame's module load a CORS preflight
// the server does not answer, so sign in the way a browser does.
test.use({ extraHTTPHeaders: {} });

test("with no query the page lists the datasets, and one opens on a sample", async ({ page }) => {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();
  await page.goto(`/${encodeURIComponent("dactalView()")}?token=${TOKEN}`);
  const frame = page.frameLocator('iframe[src="/dactal/index.html"]');
  const datasets = frame.locator("#queryoutput .datasets");

  // The working set's datasets first, `rows` leading, then the engine's own.
  await expect(datasets.locator(".dataset").first()).toHaveText(/^rows \(\d+\)$/);
  await expect(
    datasets.locator(".dataset:not(.internal)", { hasText: /^author \(/ }),
  ).toBeVisible();
  await expect(datasets.locator(".dataset.internal", { hasText: "annotators" })).toBeVisible();
  await expect(frame.locator("#dq")).toHaveValue("");
  await expect(frame.locator("#queryoutput table")).toHaveCount(0);

  await datasets.locator(".dataset").first().click();
  await expect(frame.locator("#dq")).toHaveValue("rows???sample:@<=10");
  await expect(frame.locator("#status")).toHaveText("10 results for “rows???sample:@<=10”");
  await expect(frame.locator("#queryoutput table").first()).toBeVisible();

  // Clearing the query and running it brings the list back.
  await frame.locator("#dq").fill("");
  await frame.locator("#dq").press("Enter");
  await expect(datasets.locator(".dataset").first()).toHaveText(/^rows \(\d+\)$/);
  await expect(frame.locator("#queryoutput table")).toHaveCount(0);
});
