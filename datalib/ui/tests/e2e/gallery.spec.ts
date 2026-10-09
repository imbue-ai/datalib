import { test, expect } from "@playwright/test";
import { cardOf, cardTitle, GRID, shownCards } from "./grid-helpers";

// Card creation outside edit mode goes through the new-card gallery: the
// "+" strip at the end of a Columns container adds a `galleryView()`
// card — a list of parameter-less components — and picking an entry
// REPLACES that card via host.setSource. Components that need arguments
// hide behind a picker: the gallery's "Document" entry opens
// documentPickerView (a /applet/unified_index/docs listing), which in
// turn replaces itself with `documentView("<uuid>")` on pick.

test.describe("new-card gallery (outside edit mode)", () => {
  test("+ strip → gallery → Document → picker → document card", async ({ page }) => {
    await page.goto(GRID);
    // No source boxes outside edit mode, but the "+" strip is there.
    await expect(page.locator(".ct-source")).toHaveCount(0);
    await page.locator(".ct-main .ct-add").click();

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
    await page.locator(".ct-main .ct-add").click();
    // Logs is a developer tool: listed in that section, after the views.
    const section = page.getByRole("region", { name: "Developer tools" });
    // Each group is in alphabetical order by title.
    const titles = await section.locator(".gv-row .gv-title").allTextContents();
    const listed = titles.slice(0, -1);
    expect(listed).toContain("Logs");
    expect(listed).toEqual(
      [...listed].sort((x, y) => x.localeCompare(y, undefined, { sensitivity: "base" })),
    );
    expect(titles.at(-1)).toBe("New component, built by an agent");
    await expect(
      page.locator(".gv-row", { hasText: "Markdown Document" }).and(section.locator(".gv-row")),
    ).toHaveCount(0);
    await section.locator(".gv-row", { hasText: "Logs" }).click();
    const col = shownCards(page).filter({ has: page.locator(".rl-panel") });
    await expect(col).toBeVisible({ timeout: 10_000 });
    await expect(cardTitle(col)).toHaveText("Log · everything");
    await expect(cardOf(page, "logView()")).toHaveCount(1);
  });

  test("gallery's Search entry becomes a second search", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-main .ct-add").click();
    await page
      .locator(".gv-row", { has: page.locator(".gv-title", { hasText: /^Search$/ }) })
      .click();
    // Two search cards now: the one the page opened on and the one picked.
    await expect(cardOf(page, "searchView()")).toHaveCount(2);
    await expect(page.locator(".grid-box .slickgrid-container")).toHaveCount(2, {
      timeout: 10_000,
    });
  });

  test("the gallery's Dashboard makes the card the Dashboard composite", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-main .ct-add").click();
    await page
      .locator(".gv-row", { has: page.locator(".gv-title", { hasText: /^Dashboard$/ }) })
      .click();
    await expect(cardOf(page, "galleryView()")).toHaveCount(0);
    for (const view of ["syncStatusView()", "libraryView()", "sourcesOverviewView()"]) {
      await expect(cardOf(page, view)).toHaveCount(1);
    }
  });

  test("a Dashboard section is listed among the developer tools", async ({ page }) => {
    await page.goto(GRID);
    await page.locator(".ct-main .ct-add").click();
    const library = page.locator(".gv-row", {
      has: page.locator(".gv-title", { hasText: /^Dashboard: Your library$/ }),
    });
    await expect(library).toHaveCount(1);
    await expect(
      page.getByRole("region", { name: "Developer tools" }).locator(".gv-row").and(library),
    ).toHaveCount(1);
    await library.click();
    await expect(cardOf(page, "libraryView()")).toHaveCount(1);
  });
});
