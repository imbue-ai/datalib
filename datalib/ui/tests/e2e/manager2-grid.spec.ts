// The Pipeline table on /sources2 (Manager2View) actually paints.
//
// The bug this guards is a layout one, and it is invisible to every
// other kind of assertion: `.m2-ag` sized itself with `height: 100%`
// against `.m2-grid`, whose height comes from flex resolution with no
// `height` declaration of its own. WebKit — Safari, and the WKWebView
// the Tauri desktop app runs in — resolves a percentage height against
// the parent's *specified* height, so `100%` computed to `auto` and the
// grid collapsed to its 2px of border. Rows and headers were in the DOM
// the whole time; the user saw an empty strip. Chromium resolves
// against the flexed height and looks correct, which is why the same
// mistake shipped twice (`.grid` in cards/GridCard.ce.vue was the
// first, fixed with the same absolute-fill recipe).
//
// So this spec only proves something when it runs under the `webkit`
// project — see the project list in playwright.config.ts. Under
// chromium it passes either way.
//
// Read-only: the fixture root's config.toml already declares the
// `unified_index` applet, and Manager2 lists applets alongside sources
// and plain steps, so the table has a row without this spec editing the
// config every other spec in the run shares.

import { test, expect } from "@playwright/test";
import { expectGridPainted } from "./grid-helpers";

test("the Pipeline table paints at full height", async ({ page }) => {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();

  // Rows are bound. This stayed true throughout the bug, so it is the
  // precondition — not the check.
  await expect(
    page.locator('.ag-center-cols-container [role="row"]'),
  ).not.toHaveCount(0, { timeout: 10_000 });

  // The check: the grid occupies real space on screen.
  await expectGridPainted(page.locator(".ag-root-wrapper"), "Manager2 grid");

  // Headers too — they live in their own AG Grid viewport, and a
  // container that collapses takes both with it.
  await expect(page.locator('.ag-header-cell[col-id="name"]')).toBeVisible();
});
