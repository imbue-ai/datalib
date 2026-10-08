import { test, expect } from "@playwright/test";
import { EVERY_ROW, selectRowByUuid, inDocFrame, stubClipboard } from "./grid-helpers";

// A person in a document is a chip (docs/dev/plans/chips.md). Right-click
// on one opens the chip's own menu rather than the document's; its
// entries copy the name or the identifier and open a search for
// everything from that person; a double-click opens that search
// directly. The fixture has no contacts app, so the chips are unresolved
// and the menu has no link entry — the copies and the search are what
// every root has.

type Row = { uuid: string; kind: string; message_index: number | null; author: string };

const SEARCH_INPUT = '.sc input[type="search"]';

async function openADocumentWithAChip(
  page: import("@playwright/test").Page,
  request: import("@playwright/test").APIRequestContext,
) {
  // Slack's authors all carry a handle, so any of its messages has a chip.
  const resp = await request.get("/applet/unified_index/search?q=source_id%3Aslack&limit=200");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const message = rows.find(
    (r) => !/(Chat|Thread|Reaction)$/.test(r.kind) && r.message_index != null && r.author,
  );
  expect(message, "the slack fixture must have a message row with an author").toBeTruthy();
  await page.goto(EVERY_ROW);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });
  await selectRowByUuid(page, message!.uuid);
  const chips = await inDocFrame(page, "a.chip[data-handle]");
  const chip = chips.first();
  await expect(chip).toBeVisible();
  return chip;
}

test("right-click on a chip opens its menu, and the copies carry name and identifier", async ({
  page,
  request,
}) => {
  const chip = await openADocumentWithAChip(page, request);
  const handle = (await chip.getAttribute("data-handle"))!;
  const value = handle.slice(handle.indexOf(":") + 1);
  await stubClipboard(page);

  // The menu is drawn in the app's window, over the frame.
  await chip.click({ button: "right" });
  const menu = page.locator(".chip-menu");
  await expect(menu).toBeVisible();
  await expect(menu.locator(".chip-menu-item")).toContainText([/^Copy /, /^Everything from /]);
  await menu.locator(".chip-menu-item", { hasText: `Copy ${value}` }).click();
  await expect(menu).toBeHidden();
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied))
    .toBe(value);

  // Escape closes it without picking.
  await chip.click({ button: "right" });
  await expect(menu).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(menu).toBeHidden();
});

test("the menu's search, and a double-click, open everything from the person", async ({
  page,
  request,
}) => {
  const chip = await openADocumentWithAChip(page, request);
  await expect(page.locator(SEARCH_INPUT)).toHaveCount(0);

  await chip.click({ button: "right" });
  await page.locator(".chip-menu .chip-menu-item", { hasText: /^Everything from / }).click();
  await expect(page.locator(SEARCH_INPUT)).toHaveValue(/^author:/, { timeout: 10_000 });

  // A double-click is the same search, without the menu. The chip is
  // still in the first document card.
  await chip.dblclick();
  await expect(page.locator(".chip-menu")).toHaveCount(0);
  await expect
    .poll(async () => {
      const values = await page
        .locator(SEARCH_INPUT)
        .evaluateAll((els) => els.map((e) => (e as HTMLInputElement).value));
      return values.every((v) => v.startsWith("author:")) && values.length >= 1;
    })
    .toBe(true);
});
