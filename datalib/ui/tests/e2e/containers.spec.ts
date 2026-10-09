import { test, expect, type Page } from "@playwright/test";
import { SHOWN_CARDS } from "./grid-helpers";

// The containers layout: tabs down the side, each holding cards or
// containers. The Dashboard is a solidified composite of five cards, so
// it looks like one page and a card opened from it gets a tab of its
// own; unsolidified, the card lands inside it instead. A new window has
// three tabs pinned (the Dashboard, Search, Sources): a card opened from
// one is a tab after the rest, and an open of a pinned tab's own card
// shows that tab. The tree is kept in the library, so these share one saved layout and run
// in order, each starting from a cleared one.

test.describe.configure({ mode: "serial" });

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.removeItem("datalib-layout-unsaved");
    localStorage.setItem("datalib-edit-mode", "0");
  });
  const cleared = await page.request.put("/api/ui/state/layout", {
    data: "null",
    headers: { "content-type": "application/json" },
  });
  expect(cleared.status()).toBe(204);
});

const tabs = (page: Page) => page.locator(".ct-tab");
const mainCards = (page: Page) => page.locator(SHOWN_CARDS);

// A document in the Dashboard's latest activity: opening it makes a tab.
const recentDocument = (page: Page) => mainCards(page).locator(".row.recent").first();
const PINNED = ["Dashboard", "Search", "Sources"];

type SavedTab = { name?: string | null; pinned?: boolean };

async function savedTabs(page: Page): Promise<SavedTab[] | null> {
  const r = await page.request.get("/api/ui/state/layout");
  if (!r.ok()) return null;
  const tree = (await r.json()) as { children?: SavedTab[] } | null;
  return tree?.children ?? null;
}

async function savedTabCount(page: Page): Promise<number> {
  return (await savedTabs(page))?.length ?? -1;
}

test("the Dashboard is five cards that read as one page", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".ct-tabs-pinned .ct-tab-label")).toHaveText(PINNED);
  await expect(tabs(page)).toHaveCount(3);
  await expect(tabs(page).first()).toHaveClass(/is-selected/);
  await expect(mainCards(page)).toHaveCount(5);
  await expect(page.locator(".ct-card-head, .ct-box-head")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Open Sources" })).toBeVisible();
});

test("a card opened from the Dashboard gets a tab of its own, below the pinned ones", async ({
  page,
}) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await recentDocument(page).click();
  await expect(tabs(page)).toHaveCount(4);
  await expect(page.locator(".ct-tabs-open .ct-tab")).toHaveCount(1);
  await expect(tabs(page).nth(3)).toHaveClass(/is-selected/);
  // A tab of its own, not one under the Dashboard's.
  await expect(tabs(page).nth(3)).toHaveAttribute("style", /padding-left: 0\.4rem/);
  await expect(mainCards(page)).toHaveCount(1);
  // The card fills the tab: no Columns container around it, so no card
  // header and no "+" strip.
  await expect(mainCards(page).locator(".ct-card-head")).toHaveCount(0);
  await expect(page.locator(".ct-main .ct-add")).toHaveCount(0);

  // The Dashboard kept its shape.
  await tabs(page).first().click();
  await expect(mainCards(page)).toHaveCount(5);
});

test("an open of a pinned tab's card shows that tab", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await page.getByRole("button", { name: "Open Sources" }).click();
  await expect(tabs(page).nth(2)).toHaveClass(/is-selected/);
  await expect(mainCards(page)).toHaveCount(1);
  await expect(tabs(page)).toHaveCount(3);
});

test("a tab is pinned from its menu and unpinned by its pin", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await recentDocument(page).click();
  await expect(tabs(page)).toHaveCount(4);
  const pinned = page.locator(".ct-tabs-pinned .ct-tab");
  await expect(pinned).toHaveCount(3);

  await tabs(page).nth(3).getByTitle("more").click();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await expect(pinned).toHaveCount(4);
  // A pinned tab has a pin where the others have a close.
  await expect(pinned.nth(3).getByTitle("close")).toHaveCount(0);
  await expect
    .poll(async () => (await savedTabs(page))?.map((t) => t.pinned), { timeout: 10_000 })
    .toEqual([true, true, true, true]);

  await page.reload();
  await expect(pinned).toHaveCount(4);
  await pinned.nth(1).getByTitle("unpin").click();
  await expect(pinned.locator(".ct-tab-label")).toHaveText(["Dashboard", "Sources", /./]);
  await expect(page.locator(".ct-tabs-open .ct-tab-label")).toHaveText(["Search"]);
  // Unpinned and closed, Search stays gone: the defaults are not put back.
  // A tab's close shows once the pointer is on its row.
  const search = page.locator(".ct-tabs-open .ct-tab");
  await search.hover();
  await search.getByTitle("close").click();
  await expect.poll(() => savedTabCount(page), { timeout: 10_000 }).toBe(3);
  await page.reload();
  await expect(tabs(page)).toHaveCount(3);
  await expect(pinned).toHaveCount(3);
});

test("in edit mode, unsolidifying the Dashboard opens the card inside it", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await expect(page.locator(".ct-foldertab")).toHaveCount(0);
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  // Edit mode shows the solidified container, and its menu turns that off.
  await page.locator(".ct-foldertab").click();
  const solidified = page.getByRole("menuitemcheckbox", { name: "Solidified" });
  await expect(solidified).toHaveAttribute("aria-checked", "true");
  await solidified.click();

  await page.getByRole("button", { name: "Open Sources" }).click();
  await expect(mainCards(page)).toHaveCount(6);
  await expect(tabs(page)).toHaveCount(3);
  await expect(tabs(page).first()).toHaveClass(/is-selected/);
});

test("the layout is kept in the library across a reload", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await recentDocument(page).click();
  await expect(tabs(page)).toHaveCount(4);
  await expect.poll(() => savedTabCount(page), { timeout: 10_000 }).toBe(4);

  await page.reload();
  await expect(tabs(page)).toHaveCount(4);
  await expect(tabs(page).nth(3)).toHaveClass(/is-selected/);
});

test("a tab the person renames keeps its name after a reload", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await recentDocument(page).click();
  await expect(tabs(page)).toHaveCount(4);
  await tabs(page).nth(3).getByTitle("more").click();
  await page.getByRole("menuitem", { name: "Rename…" }).click();
  await page.getByLabel("Name").fill("My reading");
  await page.getByRole("button", { name: "OK" }).click();
  await expect(tabs(page).nth(3)).toContainText("My reading");

  await expect
    .poll(async () => (await savedTabs(page))?.[3]?.name, { timeout: 10_000 })
    .toBe("My reading");
  await page.reload();
  // The card names itself again as it mounts; the person's name stays.
  await expect(mainCards(page)).toHaveCount(1);
  await expect(tabs(page).nth(3)).toContainText("My reading");
});

test("a composite cannot take a built-in composite's name", async ({ page }) => {
  await page.goto("/");
  await expect(mainCards(page)).toHaveCount(5);
  await tabs(page).first().getByTitle("more").click();
  await page.getByRole("menuitem", { name: "Save as composite…" }).click();
  await page.getByLabel("Save as composite").fill("Dashboard");
  await page.getByRole("button", { name: "OK" }).click();
  await expect(page.getByText('"Dashboard" is a built-in composite')).toBeVisible();
  await page.getByRole("button", { name: "Cancel" }).click();
  await expect(page.getByLabel("Save as composite")).toHaveCount(0);
});

// A tab switched away from used to have its cards' DOM moved out of the
// page and back, which rebuilt the Sources table's stylesheet under
// SlickGrid: its column rules went stale and every cell of a row piled
// up at one place.
test("a tab switched away from and back keeps its table drawn", async ({ page }) => {
  await page.goto("/data_sources");
  const grid = page.locator(".ct-main .tg-grid");
  const row = grid.locator(".slick-row", { hasText: "slack" }).first();
  await expect(row).toBeVisible({ timeout: 20_000 });
  // The first column is frozen, so the line is a row in each pane, joined
  // by its data-row.
  const line = grid.locator(`.slick-row[data-row="${await row.getAttribute("data-row")}"]`);
  const lefts = () =>
    line
      .locator(".slick-cell")
      .evaluateAll((cells) => cells.map((c) => Math.round(c.getBoundingClientRect().left)));
  // Every cell at a place of its own; piled up, they share one. (The
  // grid may re-create a row's cells in another order, so not in order.)
  const spread = async () => {
    const xs = await lefts();
    return xs.length > 2 && new Set(xs).size === xs.length;
  };
  await expect.poll(spread).toBe(true);

  await tabs(page).filter({ hasText: "Dashboard" }).click();
  // The tab the URL opened, not the pinned Sources.
  await tabs(page).filter({ hasText: "Sources" }).last().click();
  await expect.poll(spread).toBe(true);
});
