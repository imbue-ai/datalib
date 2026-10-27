// A message's header and the recipients line under it, as a long email
// to most of the ship draws them: chat-common's `crew_wide_email` sample
// on the render preview page (`tests/goldens/render_preview.html`), which
// is built from the document frame's own CSS and `chatSections.js` and
// which `:render_preview_test` keeps current. It needs no backend, so it
// sets the page's content rather than visiting the app.
//
// Each test is a way this layout has looked broken: a linked person's
// name nearly invisible in the recipients line, a list of thirty-six
// names pushing the message ten lines down, and the recipients showing
// through a long message's pinned header.

import { test, expect, type Locator, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";

const PREVIEW = readFileSync(
  decodeURIComponent(new URL("../goldens/render_preview.html", import.meta.url).pathname),
  "utf8",
);
const EMAIL = '[data-section-uuid="s5-roster"]';
const RECIPIENTS = 36;

async function openPreview(page: Page, colorScheme: "light" | "dark"): Promise<Locator> {
  // The preview picks its theme from the media query when it loads.
  await page.emulateMedia({ colorScheme });
  await page.setViewportSize({ width: 900, height: 700 });
  await page.setContent(PREVIEW);
  const email = page.locator(EMAIL);
  // Decorated: the long-message pass has run over it.
  await expect(email).toHaveClass(/msg--long/);
  return email;
}

type Rgba = [number, number, number, number];

function parseColor(css: string): Rgba {
  const m = css.match(/rgba?\(([^)]+)\)/);
  if (!m) throw new Error(`not an rgb() colour: ${css}`);
  const [r, g, b, a = "1"] = m[1].split(/[\s,/]+/).filter(Boolean);
  return [Number(r), Number(g), Number(b), Number(a)];
}

/** WCAG 2's contrast ratio between two opaque colours. */
function contrast(fg: Rgba, bg: Rgba): number {
  const lum = ([r, g, b]: Rgba) => {
    const ch = (c: number) => {
      const s = c / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b);
  };
  const [hi, lo] = [lum(fg), lum(bg)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

for (const scheme of ["light", "dark"] as const) {
  test(`a linked chip reads as well in the recipients line as in the header (${scheme})`, async ({
    page,
  }) => {
    const email = await openPreview(page, scheme);
    await page.locator("#linked").click();
    const chips = email.locator("h2 > a.chip, .msg-recipients a.chip");
    await expect(chips.first()).toHaveClass(/handle-resolved/);

    const looks = await chips.evaluateAll((els) =>
      els.map((el) => {
        const cs = getComputedStyle(el);
        return { name: el.textContent ?? "", color: cs.color, background: cs.backgroundColor };
      }),
    );
    expect(looks.length).toBe(RECIPIENTS + 1);
    for (const look of looks) {
      const ratio = contrast(parseColor(look.color), parseColor(look.background));
      expect(
        ratio,
        `"${look.name}": ${look.color} on ${look.background} is ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });

  test(`a long recipients list folds to two lines and opens to every name (${scheme})`, async ({
    page,
  }) => {
    const email = await openPreview(page, scheme);
    const line = email.locator(".msg-recipients");
    const toggle = email.locator("button.msg-recipients-toggle");
    await expect(toggle).toHaveText(`Show all ${RECIPIENTS}`);

    // The rows of names a reader can see: each name's top, for the names
    // that end inside the line's box.
    const visibleRows = () =>
      line.evaluate((box) => {
        const bottom = box.getBoundingClientRect().bottom;
        const names = box.querySelectorAll(".msg-recipient, a.chip");
        const tops = new Set<number>();
        let shown = 0;
        for (const n of names) {
          const r = n.getBoundingClientRect();
          if (r.bottom <= bottom + 0.5) {
            shown++;
            tops.add(Math.round(r.top));
          }
        }
        return { rows: tops.size, shown, all: names.length };
      });

    const folded = await visibleRows();
    expect(folded.all).toBe(RECIPIENTS);
    expect(folded.rows, `folded, the line shows ${folded.rows} rows of names`).toBeLessThanOrEqual(
      2,
    );
    expect(folded.shown).toBeLessThan(RECIPIENTS);

    await toggle.click();
    await expect(toggle).toHaveText("Show fewer");
    expect((await visibleRows()).shown).toBe(RECIPIENTS);
  });

  test(`a long message's pinned header hides what scrolls under it (${scheme})`, async ({
    page,
  }) => {
    const email = await openPreview(page, scheme);
    // A clamped message cannot pin its header; open it first.
    await email.locator(":scope > button.msg-expand").click();
    const header = email.locator(":scope > h2");

    for (const selected of [false, true]) {
      if (selected) {
        // The preview selects a message on a click in its body.
        await email.locator("ol, p").last().click();
        await expect(email).toHaveClass(/\bselected\b/);
      }
      // Past the recipients line, so the roster's numbered list is under
      // where the header pins rather than the gap above it.
      await email.evaluate((el) => {
        const pane = el.closest(".chat-preview")!;
        pane.scrollTop += el.getBoundingClientRect().top - pane.getBoundingClientRect().top + 150;
      });
      await expect(header).toHaveClass(/is-stuck/);

      const under = await header.evaluate((h) => {
        const r = h.getBoundingClientRect();
        const x = r.left + r.width / 2;
        const y = r.top + r.height / 2;
        const doc = h.ownerDocument;
        return {
          background: getComputedStyle(h).backgroundColor,
          // Something of the message's own is beneath the bar, or the
          // bar being see-through would show nothing.
          beneath: doc
            .elementsFromPoint(x, y)
            .some((e) => !h.contains(e) && h.parentElement!.contains(e) && e !== h.parentElement),
        };
      });
      expect(under.beneath, "nothing of the message is under the pinned header").toBe(true);
      const alpha = parseColor(under.background)[3];
      expect(
        alpha,
        `${selected ? "selected" : "unselected"}, the pinned header's background is ${under.background}`,
      ).toBe(1);
    }
  });
}
