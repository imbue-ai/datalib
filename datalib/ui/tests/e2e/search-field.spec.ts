// The search field: typing a key offers it, taking it offers its values
// from the table, and a source picked is drawn as its chip in the text,
// over the query it still is (docs/dev/plans/search_autocomplete.md).

import { test, expect, type Page } from "@playwright/test";
import { GRID, shownCards, typeInto } from "./grid-helpers";

const field = (page: Page) => shownCards(page).getByTestId("search-input");
const menu = (page: Page) => shownCards(page).locator(".cm-tooltip-autocomplete");

test("a key, then a source picked from its values, is drawn as the source's chip", async ({
  page,
}) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document");
  await field(page).press("End");
  await field(page).pressSequentially(" sou");
  await expect(menu(page).getByText("source_id:", { exact: true })).toBeVisible();
  // Tab takes the first suggestion, and taking a key asks for its values.
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:");
  const slack = menu(page).locator('a.chip[data-entity="datalib:group/slack"]');
  await expect(slack).toBeVisible();

  // Most documents first: slack's 12 ahead of slack-diff's 3.
  await field(page).pressSequentially("slac");
  await expect(menu(page).locator("li").first().locator("a.chip")).toHaveAttribute(
    "data-entity",
    "datalib:group/slack",
  );
  await field(page).press("Tab");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:slack ");
  await expect(field(page).locator('a.chip[data-entity="datalib:group/slack"]')).toBeVisible();

  // Backspace at the chip's end takes it whole; the key stays.
  await field(page).press("End");
  await field(page).press("Backspace");
  await field(page).press("Backspace");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:");
});

/// Enter with nothing chosen searches what was typed: a value taken by
/// hand is a search, not a pick.
test("Enter with no suggestion chosen leaves the typed value as it is", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "");
  await field(page).pressSequentially("source_id:sla");
  await expect(menu(page)).toBeVisible();
  await field(page).press("Enter");
  await expect(field(page)).toHaveAttribute("data-query", "source_id:sla");
});

const slackChip = (page: Page) => field(page).locator('a.chip[data-entity="datalib:group/slack"]');

/// One click selects a chip whole: typing replaces it, and what was
/// typed stays text until the cursor leaves it.
test("a click selects a chip, and typing replaces it", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source_id:slack ");
  await slackChip(page).click();
  await expect(slackChip(page)).toHaveClass(/cm-chip-selected/);
  await page.keyboard.type("gith");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:gith ");
  await expect(slackChip(page)).toHaveCount(0);
});

/// A double-click opens a chip as its text, the value selected and the
/// key's values offered.
test("a double-click opens a chip to be edited", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source_id:slack ");
  await slackChip(page).dblclick();
  await expect(slackChip(page)).toHaveCount(0);
  await expect(field(page)).toContainText("source_id:slack");
  await expect(menu(page)).toBeVisible();
  // The value is selected, so what is typed replaces it.
  await page.keyboard.type("x");
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:x ");
});

/// A right-click is the chip's menu: the field's entries, then the chip's
/// own. Exclude adds the dash and keeps the chip.
test("a chip's menu excludes what it names, and opens it as text", async ({ page }) => {
  await page.goto(GRID);
  await typeInto(field(page), "is:document source_id:slack ");
  await slackChip(page).click({ button: "right" });
  const chipMenu = shownCards(page).locator(".cm-chip-menu");
  await expect(chipMenu.getByRole("menuitem")).toContainText([
    "Edit as text",
    /^Exclude /,
    /^Copy /,
  ]);
  await chipMenu.getByRole("menuitem", { name: /^Exclude / }).click();
  await expect(field(page)).toHaveAttribute("data-query", "is:document -source_id:slack ");
  await expect(slackChip(page)).toBeVisible();

  await slackChip(page).click({ button: "right" });
  await chipMenu.getByRole("menuitem", { name: /^Include / }).click();
  await expect(field(page)).toHaveAttribute("data-query", "is:document source_id:slack ");
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
