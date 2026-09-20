// A toast is drawn over everything, so it must take the pointer nowhere
// but its own ×. The case that found this: a sticky error toast from a
// failed grid search landed on the wizard's "Add source" button, and
// every click on the button went to the toast instead (the onboarding
// spec retried it for 120s). Nothing here writes the config: the
// primary button is checked with a trial click, and the real click goes
// to Cancel beside it.
import { test, expect, type Locator } from "@playwright/test";
import { stubClipboard } from "./grid-helpers";

// Small enough that the dialog reaches its max height and its footer
// sits where the toast tray is.
test.use({ viewport: { width: 1000, height: 560 } });

async function overlaps(a: Locator, b: Locator): Promise<boolean> {
  const [ra, rb] = await Promise.all([a.boundingBox(), b.boundingBox()]);
  if (!ra || !rb) return false;
  return (
    ra.x < rb.x + rb.width &&
    rb.x < ra.x + ra.width &&
    ra.y < rb.y + rb.height &&
    rb.y < ra.y + ra.height
  );
}

test("an error toast over the wizard does not eat clicks on its buttons", async ({ page }) => {
  // The failure that produced the toast: the applet gateway answering
  // the grid's first search with a 502. A grid card sits beside the
  // sources card so there is a search to fail; the sources card alone
  // searches nothing.
  await page.route("**/applet/unified_index/search**", (route) =>
    route.fulfill({
      status: 502,
      contentType: "application/json",
      body: JSON.stringify({ error: 'applet "unified_index": it is not running' }),
    }),
  );
  await page.goto("/sourcesView():1.6/gridView()");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  const toast = page.locator(".datalib-toast--error", { hasText: "unified_index/search" });
  await expect(toast).toContainText("→ 502");

  await page.getByRole("button", { name: "+ Data Source" }).click();
  await page.getByRole("searchbox").fill("whatsapp");
  await page.getByRole("button", { name: /WhatsApp/ }).click();
  const wizard = page.getByRole("dialog");
  await wizard.locator("input.wiz-path").fill("/Users/x/backups/WhatsApp");

  const submit = wizard.getByRole("button", { name: "Add source" });
  await expect(submit).toBeEnabled();
  // The geometry under test: the toast really is on top of the button.
  expect(await overlaps(toast, submit), "the toast must cover the primary button").toBe(true);

  // Playwright's actionability check is the same hit test a click
  // performs; a trial click passes it without writing anything.
  await submit.click({ trial: true, timeout: 5_000 });
  // Whatever the toast covers is what a click reaches.
  const box = (await toast.locator(".datalib-toast__msg").boundingBox())!;
  const hit = await page.evaluate(
    ([x, y]) => document.elementFromPoint(x, y)?.closest(".datalib-toast") !== null,
    [box.x + box.width / 2, box.y + box.height / 2],
  );
  expect(hit, "the toast body took the pointer").toBe(false);

  // Its two buttons are what it does own. Copy stands in for the text
  // selection the pass-through gives up.
  const readClipboard = await stubClipboard(page);
  await toast.getByRole("button", { name: "Copy" }).click({ timeout: 5_000 });
  await expect.poll(readClipboard).toContain("unified_index/search");
  await expect(toast.getByRole("button", { name: "Copied" })).toBeVisible();
  await toast.getByRole("button", { name: "Dismiss" }).click({ timeout: 5_000 });
  await expect(toast).toHaveCount(0);

  await wizard.getByRole("button", { name: "Cancel" }).click({ timeout: 5_000 });
  await expect(wizard).toHaveCount(0);
});
