import { test, expect } from "@playwright/test";
import { cardOf, shownCards, shownTabName, stubClipboard, tabLabels } from "./grid-helpers";

// The chrome around the cards: the toolbar's search box opens a search
// card on what was typed, and ⌘K (Ctrl+K) reaches it from anywhere;
// the status bar's "Logs" reveals the log once. Each opens a tab of
// its own. A new window opens on the Dashboard.

const searchBox = (page: import("@playwright/test").Page) =>
  page.getByRole("searchbox", { name: "Search your data" });

test.describe("toolbar", () => {
  test("a new window opens on the Dashboard", async ({ page }) => {
    await page.goto("/");
    await expect(tabLabels(page)).toHaveText(["Dashboard"]);
    await expect(shownCards(page)).toHaveCount(5);
  });

  test("the search box opens a search card on what was typed", async ({ page }) => {
    await page.goto("/");
    await expect(tabLabels(page)).toHaveText(["Dashboard"]);
    await searchBox(page).fill("warp");
    await searchBox(page).press("Enter");
    const card = cardOf(page, 'searchView({"q":"warp"})');
    await expect(card).toBeVisible();
    await expect(shownTabName(page)).toHaveText("Search: warp");
    await expect(tabLabels(page)).toHaveCount(2);
    // The box empties, ready for the next search.
    await expect(searchBox(page)).toHaveValue("");
  });

  test("Ctrl+K puts the caret in the search box", async ({ page }) => {
    await page.goto("/");
    await expect(searchBox(page)).not.toBeFocused();
    await page.keyboard.press("Control+k");
    await expect(searchBox(page)).toBeFocused();
  });

  test("the status bar's Logs opens the log over every run, once", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Logs" }).click();
    const col = shownCards(page).filter({ has: page.locator(".rl-panel") });
    await expect(col).toBeVisible({ timeout: 10_000 });
    await expect(shownTabName(page)).toHaveText("Log · everything");
    await expect(cardOf(page, "logView()")).toHaveCount(1);

    await page.getByRole("button", { name: "Logs" }).click();
    await expect(tabLabels(page)).toHaveCount(2);
    await expect(cardOf(page, "logView()")).toHaveCount(1);
  });

  test("the status bar's density resizes the chrome, and is kept", async ({ page }) => {
    await page.goto("/");
    const toolbar = page.locator(".datalib-toolbar");
    const group = page.getByRole("group", { name: "density" });
    const slider = group.getByRole("slider", { name: "Density" });
    await expect(slider).toHaveValue("0");
    await expect(group.getByRole("button", { name: "More compact" })).toBeDisabled();
    const before = (await toolbar.boundingBox())!.height;

    for (let i = 0; i < 4; i++) await group.getByRole("button", { name: "More spacious" }).click();
    await expect(slider).toHaveValue("0.5");
    expect((await toolbar.boundingBox())!.height).toBeGreaterThan(before);

    // The slider sets a step directly, and the step is kept.
    await slider.fill("0.75");
    await expect(slider).toHaveValue("0.75");
    await page.reload();
    await expect(slider).toHaveValue("0.75");
  });

  test("the status bar copies the data root's path in a browser", async ({ page }) => {
    await page.goto("/");
    const path = page.getByTestId("root-storage").locator(".root-bar-path");
    await expect(path).not.toHaveText("", { timeout: 10_000 });
    const copied = await stubClipboard(page);
    await page.getByRole("button", { name: "Copy path" }).click();
    await expect(page.locator(".datalib-toast", { hasText: "path copied" })).toBeVisible();
    expect(await copied()).toBe(await path.textContent());
  });

  // The window's minimum width in the desktop shell (MIN_WINDOW_WIDTH)
  // assumes the search box shrinks first and to no less than 180px, and
  // that the library name then ellipsizes rather than sliding under it.
  test("the search box sits at the right end and shrinks with the window", async ({ page }) => {
    await page.goto("/");
    const box = page.locator(".command-box");
    const lib = page.locator(".crumb-lib");
    const name = page.locator(".crumb-name");
    await expect(box).toBeVisible();
    const wide = (await box.boundingBox())!;
    expect(wide.width).toBe(440);
    expect(1280 - (wide.x + wide.width)).toBeLessThan(16);
    // Whether the name is cut short: its text's width against its box's,
    // both fractional. Not `scrollWidth > clientWidth`: those round
    // differently, and a name that happens to measure n.5px reads as
    // clipped when nothing is.
    const truncated = () =>
      name.evaluate((el) => {
        const text = document.createRange();
        text.selectNodeContents(el);
        return text.getBoundingClientRect().width > el.getBoundingClientRect().width + 0.5;
      });

    // Density 0, then 0.5 (four steps more spacious).
    for (const larger of [0, 4]) {
      await page.setViewportSize({ width: 1280, height: 800 });
      for (let i = 0; i < larger; i++)
        await page.getByRole("button", { name: "More spacious" }).click();
      // 700px leaves the name room whatever its random suffix measures;
      // 600px was within a few letters of it with larger text.
      for (const width of [700, 420]) {
        await page.setViewportSize({ width, height: 800 });
        const b = (await box.boundingBox())!;
        const l = (await lib.boundingBox())!;
        expect(b.width).toBeLessThan(440);
        expect(b.width).toBeGreaterThanOrEqual(180);
        expect(b.x + b.width).toBeLessThanOrEqual(width);
        expect(b.x).toBeGreaterThanOrEqual(l.x + l.width);
        // At 700px the search box has room to give; at 420px it is at
        // its floor and the name gives way.
        await expect.poll(truncated).toBe(width === 420);
      }
    }
  });

  test("the syncing pill is absent when nothing runs", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".datalib-toolbar")).toBeVisible();
    await expect(page.locator(".sync-indicator")).toHaveCount(0);
  });
});
