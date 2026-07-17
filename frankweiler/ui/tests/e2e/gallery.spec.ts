import { test, expect } from "@playwright/test";

// Non-dev card creation goes through the new-card gallery: the "+"
// strip after the last miller column creates a `galleryView()` card —
// a list of parameter-less components — and picking an entry REPLACES
// that card via host.setSource. Components that need arguments hide
// behind a picker: the gallery's "Document" entry opens
// documentPickerView (a /api/docs listing), which in turn replaces
// itself with `documentView("<uuid>")` on pick.
//
// Dev mode is off by default (fresh browser context), so these tests
// exercise exactly the non-dev affordances.

test.describe("new-card gallery (non-dev mode)", () => {
  test("+ strip → gallery → Document → picker → document card", async ({
    page,
  }) => {
    await page.goto("/");
    // Non-dev: no source boxes, but the "+" creation strip is there.
    await expect(page.locator(".miller-col-source")).toHaveCount(0);
    await page.locator(".miller-add").click();

    // The gallery column appears, builtins listed with gridView first.
    const galleryRows = page.locator(".gv-row");
    await expect(galleryRows.first()).toContainText("Search");
    expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toContain(
      "galleryView()",
    );

    // Pick "Document" → the gallery card becomes the document picker.
    await galleryRows.filter({ hasText: "Document" }).first().click();
    const docRows = page.locator(".dp-row");
    await expect(docRows.first()).toBeVisible({ timeout: 10_000 });
    expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toContain(
      "documentPickerView()",
    );

    // Pick the first document → the picker becomes that document.
    await docRows.first().click();
    await expect(page.locator(".chat-preview")).toBeVisible({ timeout: 10_000 });
    expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toContain(
      'documentView("',
    );
  });

  test("gallery's Search entry becomes a second grid", async ({ page }) => {
    await page.goto("/");
    await page.locator(".miller-add").click();
    await page.locator(".gv-row", { hasText: "Search" }).first().click();
    // Two grid columns now: the default one and the freshly picked one.
    await expect(page.locator(".ag-root-wrapper")).toHaveCount(2, {
      timeout: 10_000,
    });
    expect(decodeURIComponent(await page.evaluate(() => location.pathname))).toContain(
      "gridView()",
    );
  });
});
