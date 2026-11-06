// The search field: typing a key offers it, taking it offers its values
// from the table, and a source picked is drawn as its chip in the text,
// over the query it still is (docs/dev/plans/search_autocomplete.md).

import { test, expect, type Page } from "@playwright/test";
import { cardOf, GRID, SEARCH_SETTLE, typeInto } from "./grid-helpers";

const field = (page: Page) => page.getByTestId("search-input");
const menu = (page: Page) => page.locator(".cm-tooltip-autocomplete");
const onSearchCard = (page: Page) => page.locator(".command-box");
const shownQuery = (page: Page) => cardOf(page, "searchView()").locator(".grid-wrap");

/// The toolbar's box is up before the app's first read of the config lands,
/// and no card mounts until it does. Holding that read keeps the search card
/// away for as long as a spec needs; the function it returns lets it through.
async function holdTheCards(page: Page): Promise<() => void> {
  let letThrough!: () => void;
  const mayLand = new Promise<void>((land) => (letThrough = land));
  await page.route("**/api/config", async (route) => {
    await mayLand;
    await route.continue();
  });
  return letThrough;
}

/// The search card arriving wrote its query over whatever was in the box, so
/// someone quick lost what they typed, and specs that typed straight after a
/// page load failed on a box holding `is:document`.
test("what is typed while the app loads stays when the search card arrives", async ({ page }) => {
  const letThrough = await holdTheCards(page);
  await page.goto(GRID);
  await typeInto(field(page), "warp");
  await expect(field(page)).toHaveAttribute("data-query", "warp");
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "false");

  letThrough();
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "true");
  await expect(field(page)).toHaveAttribute("data-query", "warp");
  // It is still a draft: the card shows its own search until Enter.
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "is:document", {
    timeout: SEARCH_SETTLE,
  });
  await field(page).press("Enter");
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "warp", {
    timeout: SEARCH_SETTLE,
  });
  await expect(field(page)).toHaveAttribute("data-query", "warp");
});

/// Enter before there is a card runs the search once there is one, and the box
/// then shows what ran; what is typed after that Enter is a new draft, and stays.
test("Enter while the app loads runs the search when the search card arrives", async ({ page }) => {
  const letThrough = await holdTheCards(page);
  await page.goto(GRID);
  await typeInto(field(page), "source:slack ");
  await field(page).press("Enter");
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "false");

  letThrough();
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "source:slack", {
    timeout: SEARCH_SETTLE,
  });
  await expect(field(page)).toHaveAttribute("data-query", "source:slack");
});

test("what is typed after an Enter while the app loads stays too", async ({ page }) => {
  const letThrough = await holdTheCards(page);
  await page.goto(GRID);
  await typeInto(field(page), "source:slack");
  await field(page).press("Enter");
  await typeInto(field(page), "source:slack warp");
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "false");

  letThrough();
  await expect(shownQuery(page)).toHaveAttribute("data-shown-query", "source:slack", {
    timeout: SEARCH_SETTLE,
  });
  await expect(field(page)).toHaveAttribute("data-query", "source:slack warp");
});

/// Someone who has clicked in and not typed yet gets the card's query, and
/// typing replaces it rather than landing on the end of it.
test("typing replaces the query a search card brought to a box already clicked in", async ({
  page,
}) => {
  const letThrough = await holdTheCards(page);
  await page.goto(GRID);
  await field(page).click();
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "false");

  letThrough();
  await expect(onSearchCard(page)).toHaveAttribute("data-on-search-card", "true");
  await expect(field(page)).toHaveAttribute("data-query", "is:document");
  await page.keyboard.type("warp");
  await expect(field(page)).toHaveAttribute("data-query", "warp");
});

test("a key, then a source picked from its values, is drawn as the source's chip", async ({
  page,
}) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document");
  await field(page).press("End");
  await field(page).pressSequentially(" sou");
  await expect(menu(page).getByText("source:", { exact: true })).toBeVisible();
  // Tab takes the first suggestion, and taking a key asks for its values.
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:");
  const slack = menu(page).locator('a.chip[data-entity="datalib:group/slack"]');
  await expect(slack).toBeVisible();

  // Most documents first: slack's 12 ahead of slack-diff's 3.
  await field(page).pressSequentially("slac");
  await expect(menu(page).locator("li").first().locator("a.chip")).toHaveAttribute(
    "data-entity",
    "datalib:group/slack",
  );
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:slack ");
  await expect(field(page).locator('a.chip[data-entity="datalib:group/slack"]')).toBeVisible();

  // Backspace at the chip's end takes it whole; the key stays.
  await field(page).press("End");
  await field(page).press("Backspace");
  await field(page).press("Backspace");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:");
});

/// Enter with nothing chosen searches what was typed: a value taken by
/// hand is a search, not a pick. The suggestions close with it; they
/// used to stay open over the results.
test("Enter with no suggestion chosen leaves the typed value as it is", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "");
  await field(page).pressSequentially("source:sla");
  await expect(menu(page)).toBeVisible();
  await field(page).press("Enter");
  await expect(field(page)).toHaveAttribute("data-query", "source:sla");
  await expect(menu(page)).toBeHidden();
});

const slackChip = (page: Page) => field(page).locator('a.chip[data-entity="datalib:group/slack"]');

/// One click selects a chip whole: typing replaces it, and what was
/// typed stays text until the cursor leaves it.
test("a click selects a chip, and typing replaces it", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source:slack ");
  await slackChip(page).click();
  await expect(slackChip(page)).toHaveClass(/cm-chip-selected/);
  await page.keyboard.type("gith");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:gith ");
  await expect(slackChip(page)).toHaveCount(0);
});

/// A double-click opens a chip as its text, the value selected and the
/// key's values offered.
test("a double-click opens a chip to be edited", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source:slack ");
  await slackChip(page).dblclick();
  await expect(slackChip(page)).toHaveCount(0);
  await expect(field(page)).toContainText("source:slack");
  await expect(menu(page)).toBeVisible();
  // The value is selected, so what is typed replaces it.
  await page.keyboard.type("x");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:x ");
});

/// A right-click is the chip's menu: the field's entries, then the chip's
/// own. Exclude adds the dash and keeps the chip.
test("a chip's menu excludes what it names, and opens it as text", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source:slack ");
  await slackChip(page).click({ button: "right" });
  const chipMenu = page.locator(".cm-chip-menu");
  await expect(chipMenu.getByRole("menuitem")).toContainText([
    "Edit as text",
    /^Exclude /,
    /^Copy /,
  ]);
  await chipMenu.getByRole("menuitem", { name: /^Exclude / }).click();
  await expect(field(page)).toHaveAttribute("data-query", "is:document -source:slack ");
  await expect(slackChip(page)).toBeVisible();

  await slackChip(page).click({ button: "right" });
  await chipMenu.getByRole("menuitem", { name: /^Include / }).click();
  await expect(field(page)).toHaveAttribute("data-query", "is:document source:slack ");
  await slackChip(page).click({ button: "right" });
  await chipMenu.getByRole("menuitem", { name: "Edit as text" }).click();
  await expect(slackChip(page)).toHaveCount(0);
  await expect(menu(page)).toBeVisible();
});

/// A person key offers handles as the person's chip: picking one is an
/// exact search on that handle, drawn as their chip in the text.
test("a person picked for from: is drawn as their chip", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "from:riker@enter");
  const riker = menu(page).locator('a.chip[data-handle="email:riker@enterprise.starfleet"]');
  await expect(riker).toBeVisible();
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "from:email:riker@enterprise.starfleet ");
  await expect(
    field(page).locator('a.chip[data-handle="email:riker@enterprise.starfleet"]'),
  ).toBeVisible();
});

/// A name picked for a person key is written quoted: matched whole, where
/// the same name typed bare matches any name holding it. The name is one no
/// handle goes by (a Gemini answer's author); a person with a handle is
/// offered as their chip first.
test("a name picked for from: is quoted, to be matched whole", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "from:gemin");
  await expect(menu(page).getByText("Gemini", { exact: true })).toBeVisible();
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", 'from:"Gemini" ');
});

/// Text put in the field before the keys arrive, as a paste is, had no
/// menu: nothing asked again when they landed.
test("what was typed before the keys arrived gets its menu when they do", async ({ page }) => {
  let letKeysLand!: () => void;
  const keysMayLand = new Promise<void>((land) => (letKeysLand = land));
  await page.route("**/search/keys", async (route) => {
    await keysMayLand;
    await route.continue();
  });
  await page.goto(GRID);
  await typeInto(field(page), "from:gemin");
  await expect(field(page)).toHaveAttribute("data-query", "from:gemin");
  // Long enough for the menu to have asked and found no keys, which shows
  // on nothing to wait for. Too short only means this test proves less: it
  // cannot go red on a slow machine, since the keys are then there first.
  await page.waitForTimeout(500);
  letKeysLand();
  await expect(menu(page).getByText("Gemini", { exact: true })).toBeVisible();
});

/// A Tab pressed while the values for what was just typed are still on
/// their way, as on a loaded machine, waits for them and takes the first,
/// rather than doing nothing and moving focus away.
test("a Tab while the values are on their way takes the first of them", async ({ page }) => {
  await page.route(/\/values\?.*typed=slac(&|$)/, async (route) => {
    await new Promise((r) => setTimeout(r, 1500));
    await route.continue();
  });
  await page.goto(GRID);
  await typeInto(field(page), "source:");
  await expect(menu(page).locator('a.chip[data-entity="datalib:group/slack"]')).toBeVisible();
  await field(page).pressSequentially("slac");
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "source:slack ");
});

/// A date menu's search, `after:… before:…`, was plain text in the field,
/// where every other date in the app is a chip.
test("a date typed for a date key is drawn as a date chip", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "");
  await field(page).pressSequentially("after:2026-01-05 ");
  await expect(field(page)).toHaveAttribute("data-query", "after:2026-01-05 ");
  await expect(field(page).locator("time.date-chip")).toHaveText("Mon, Jan 5, 2026");
});
