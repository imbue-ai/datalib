import { test, expect } from "@playwright/test";
import { cardOf, GRID, shownCards, shownTabName, tabLabels } from "./grid-helpers";

// Card creation outside edit mode goes through the new-card gallery: the
// sidebar's "New card" opens a tab holding a `galleryView()` card — a list of parameter-less components — and picking an entry
// REPLACES that card via host.setSource. Components that need arguments
// hide behind a picker: the gallery's "Document" entry opens
// documentPickerView (a /applet/unified_index/docs listing), which in
// turn replaces itself with `documentView("<uuid>")` on pick.

test.describe("new-card gallery (outside edit mode)", () => {
  test("New card → gallery → Document → picker → document card", async ({ page }) => {
    await page.goto(GRID);
    // No source boxes outside edit mode.
    await expect(page.locator(".ct-source")).toHaveCount(0);
    await page.locator(".ct-new").click();

    // The gallery card appears, builtins listed with Dashboard first.
    const galleryRows = page.locator(".gv-row");
    await expect(galleryRows.first()).toContainText("Dashboard");
    await expect(cardOf(page, "galleryView()")).toHaveCount(1);

    // Pick "Markdown Document" → the gallery card becomes the document picker.
    await galleryRows.filter({ hasText: "Markdown Document" }).first().click();
    const docRows = page.locator(".dp-row");
    await expect(docRows.first()).toBeVisible({ timeout: 10_000 });
    await expect(cardOf(page, "documentPickerView()")).toHaveCount(1);
    await expect(cardOf(page, "galleryView()")).toHaveCount(0);

    // Pick the first document → the picker becomes that document.
    await docRows.first().click();
    await expect(page.locator(".chat-preview")).toBeVisible({ timeout: 10_000 });
    await expect(cardOf(page, 'documentView("')).toHaveCount(1);
  });

  test("gallery's Logs entry becomes a log card over every run", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-new").click();
    await page.locator(".gv-row", { hasText: "Logs" }).first().click();
    const col = shownCards(page).filter({ has: page.locator(".rl-panel") });
    await expect(col).toBeVisible({ timeout: 10_000 });
    await expect(shownTabName(page)).toHaveText("Log · everything");
    await expect(cardOf(page, "logView()")).toHaveCount(1);
  });

  test("gallery's Unified Search entry becomes a second grid", async ({ page }) => {
    await page.goto(GRID);
    await expect(cardOf(page, "gridView()")).toHaveCount(1);
    const before = await tabLabels(page).count();
    await page.locator(".ct-new").click();
    // By its exact title: "Unified Search (new)" is the Search card.
    await page
      .locator(".gv-row", { has: page.locator(".gv-title", { hasText: /^Unified Search$/ }) })
      .click();
    // Two grid cards now, each a tab: the default one, still mounted
    // behind, and the freshly picked one.
    await expect(page.locator(".grid-box .slickgrid-container")).toHaveCount(2, {
      timeout: 10_000,
    });
    await expect(cardOf(page, "gridView()")).toHaveCount(1);
    await expect(tabLabels(page)).toHaveCount(before + 1);
  });

  test("the gallery's Dashboard makes the card the Dashboard composite", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-new").click();
    await page
      .locator(".gv-row", { has: page.locator(".gv-title", { hasText: /^Dashboard$/ }) })
      .click();
    await expect(cardOf(page, "galleryView()")).toHaveCount(0);
    for (const view of ["syncStatusView()", "libraryView()", "sourcesOverviewView()"]) {
      await expect(cardOf(page, view)).toHaveCount(1);
    }
  });

  test("a Dashboard section is listed only once the gallery shows every view", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-new").click();
    const library = page.locator(".gv-row", {
      has: page.locator(".gv-title", { hasText: /^Your library$/ }),
    });
    await expect(page.locator(".gv-row").first()).toBeVisible();
    await expect(library).toHaveCount(0);
    await page.getByLabel("Show every view").check();
    await expect(library).toHaveCount(1);
    await library.click();
    await expect(cardOf(page, "libraryView()")).toHaveCount(1);
  });
});
