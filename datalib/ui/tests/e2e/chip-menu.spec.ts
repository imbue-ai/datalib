import { test, expect } from "@playwright/test";
import { EVERY_ROW, selectRowByUuid, inDocFrame, stubClipboard } from "./grid-helpers";

// A person in a document is a chip (docs/dev/chips.md). Right-click
// on one opens the chip's own menu rather than the document's; its
// entries copy the name or the identifier and open a search for
// everything from that person; a double-click opens the person's card,
// led by this document's source. The fixture has no contacts app, so
// the chips are unresolved and the menu has no link entry — the copies,
// the search and the card are what every root has.

type Row = { uuid: string; kind: string; message_index: number | null; author: string };

// The search box of the Search card an "everything from" opens (its source
// names a query), not that of the search the page opened on.
const SEARCH_INPUT = '.ct-card[data-card-source*="searchView({"] [data-testid="search-input"]';

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

/// The document's `img` rule, sized `auto` for attachments, outranked the
/// mark's own size: a Slack chip's SVG grew to the layout, overlapping
/// the words beside it and swelling to a hand's width under the pointer.
test("a chip's mark or photo stays the size of a letter, hovered or not", async ({
  page,
  request,
}) => {
  const chip = await openADocumentWithAChip(page, request);
  const leadSize = () =>
    chip.evaluate((a) => {
      const lead = a.querySelector("img")!.getBoundingClientRect();
      const fontPx = parseFloat(getComputedStyle(a).fontSize);
      return { w: lead.width / fontPx, h: lead.height / fontPx };
    });
  for (const hovered of [false, true]) {
    if (hovered) await chip.hover();
    const { w, h } = await leadSize();
    expect(h, `lead height in ems, hovered: ${hovered}`).toBeGreaterThan(0);
    expect(h, `lead height in ems, hovered: ${hovered}`).toBeLessThanOrEqual(1.5);
    expect(w, `lead width in ems, hovered: ${hovered}`).toBeLessThanOrEqual(1.5);
  }
});

test("the menu's search opens everything from the person", async ({ page, request }) => {
  const chip = await openADocumentWithAChip(page, request);
  await expect(page.locator(SEARCH_INPUT)).toHaveCount(0);

  await chip.click({ button: "right" });
  await page.locator(".chip-menu .chip-menu-item", { hasText: /^Everything from / }).click();
  // By the handle itself, which the search bar draws as the person's chip.
  await expect(page.locator(SEARCH_INPUT)).toHaveAttribute("data-query", /^from:slack:/, {
    timeout: 10_000,
  });
});

test("a double-click opens the person's card, led by the document's source", async ({
  page,
  request,
}) => {
  const chip = await openADocumentWithAChip(page, request);
  await chip.dblclick();
  await expect(page.locator(".chip-menu")).toHaveCount(0);

  const card = page.locator(".person");
  await expect(card.locator(".person-name")).not.toHaveText("", { timeout: 10_000 });
  await expect(card.locator(".person-about")).toContainText("not linked to a contact");
  // Slack's record of its own author, from the document's source.
  const first = card.locator(".person-section").first();
  await expect(first).toHaveClass(/person-seen-here/);
  await expect(first.locator(".person-badge")).toHaveText("seen here");

  // Everything from them is a search, opened beside the card.
  await card.getByRole("button", { name: "Everything from them" }).click();
  // By the handle itself, which the search bar draws as the person's chip.
  await expect(page.locator(SEARCH_INPUT)).toHaveAttribute("data-query", /^from:slack:/, {
    timeout: 10_000,
  });
});
