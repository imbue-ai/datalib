// Help text in the Add Data Source wizard can be selected and copied.
// Every field is a <label>, and in WebKit — the desktop app's engine —
// the click that ends a drag across the help activated the label: focus
// jumped to the input and the selection vanished. Chromium skips label
// activation after a selection, so this spec earns its place in the
// webkit project.

import { test, expect } from "@playwright/test";

test("dragging across a field's help keeps the selection", async ({ page }) => {
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  await page.locator(".wiz-filter").fill("messages");
  await page.getByRole("button", { name: /Apple Messages/ }).click();

  const wizard = page.getByRole("dialog");
  const help = wizard.locator(".wiz-help", { hasText: "Choose it with the picker" });
  await expect(help).toBeVisible();

  // The folder the picker opens at is named in the help, copyable.
  await expect(wizard.getByRole("button", { name: "Copy ~/Library/Messages" })).toBeVisible();

  // The drag runs across the help's longest run of text, located by the
  // characters themselves: where the copy button and the line breaks
  // fall depends on the platform's fonts, and a press on the button is
  // a click, not a drag.
  const { from, to } = await help.evaluate((el) => {
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let longest: Text | null = null;
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (!longest || n.textContent!.length > longest.textContent!.length) longest = n as Text;
    }
    const range = document.createRange();
    range.setStart(longest!, 2);
    range.setEnd(longest!, Math.min(longest!.length - 2, 80));
    const rects = [...range.getClientRects()];
    const [first, last] = [rects[0], rects[rects.length - 1]];
    return {
      from: { x: first.left + 1, y: first.top + first.height / 2 },
      to: { x: last.right - 1, y: last.top + last.height / 2 },
    };
  });
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(to.x, to.y, { steps: 8 });
  await page.mouse.up();

  await expect
    .poll(() => page.evaluate(() => String(window.getSelection()).length))
    .toBeGreaterThan(20);
  await expect(wizard.locator("input.wiz-path")).not.toBeFocused();
});
