// A failed search is shown in the grid card, as a sentence: not toasted,
// not as the raw URL and JSON body, and with the previous query's rows,
// which stay painted, marked as such. The case that found this was the
// gateway's 30s timeout, which reached the card as "error: …/search?… →
// 502: {"error":"read response: Resource temporarily unavailable (os
// error 35)"}" above rows that answered a different query.
import { test, expect } from "@playwright/test";
import { SEARCH_ROWS } from "./grid-helpers";

test("a timed-out search says so, marks the old rows, and retries", async ({ page }) => {
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 10_000 });

  let fail = true;
  await page.route("**/applet/unified_index/search**", (route) =>
    fail
      ? route.fulfill({
          status: 504,
          contentType: "application/json",
          body: JSON.stringify({ error: 'applet "unified_index" did not answer within 30s' }),
        })
      : route.fallback(),
  );
  await page.getByTestId("search-input").fill("is:document");

  const banner = page.getByRole("alert").filter({ hasText: "Search timed out" });
  await expect(banner).toContainText(
    'Search timed out: applet "unified_index" did not answer within 30s.',
  );
  await expect(banner).toContainText("The rows below are from the previous search.");
  await expect(page.locator(".grid--stale")).toHaveCount(1);
  await expect(page.locator(".datalib-toast", { hasText: "unified_index/search" })).toHaveCount(0);

  fail = false;
  await banner.getByRole("button", { name: "Retry" }).click();
  await expect(banner).toHaveCount(0);
  await expect(page.locator(".grid--stale")).toHaveCount(0);
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible();
});
