import { test, expect } from "@playwright/test";
import { clickRowByUuid } from "./grid-helpers";

// The focused message in the document pane gets a visible
// "highlight window" — an accent-colored outline on all four
// sides — so the user can tell at a glance which message they
// clicked. This styling was lost once before (QMD rewrite) and
// restored; this test pins the outline so it doesn't silently
// regress again.

test("selected message has a visible accent-colored outline", async ({
  page,
  request,
}) => {
  const resp = await request.get("/api/search?q=&limit=1000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as {
    rows: {
      uuid: string;
      conversation_uuid: string;
      kind: string;
      message_index: number | null;
    }[];
  };
  const pick = data.rows.find(
    (r) => r.kind !== "Chat" && r.message_index != null,
  );
  expect(pick, "fixture must contain a message row").not.toBeUndefined();

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  // Match on row uuid, not (conversation_uuid, message_index). Some
  // providers shard a conversation into multiple rendered files
  // (beeper renders one per period), so several rows can share
  // (conversation_uuid, message_index=0) — one per period. Matching
  // by uuid guarantees we click exactly the row whose uuid we then
  // assert against.
  await clickRowByUuid(page, pick!.uuid);

  const selected = page.locator(
    `.chat-preview [data-section-uuid="${pick!.uuid}"].selected`,
  );
  await expect(selected).toBeVisible({ timeout: 10_000 });

  // Verify the outline is actually drawn — non-zero width and a
  // non-transparent color. We don't pin the *exact* color because
  // it comes from the `--fw-accent` CSS var which the theme can
  // legitimately retune. But a missing outline ("none" or "0px")
  // is the regression we care about.
  const outline = await selected.evaluate((el) => {
    const cs = getComputedStyle(el);
    return {
      style: cs.outlineStyle,
      width: cs.outlineWidth,
      color: cs.outlineColor,
    };
  });
  expect(outline.style, `outline-style should not be 'none'`).not.toBe("none");
  const widthPx = parseFloat(outline.width);
  expect(widthPx, `outline-width should be > 0px (got ${outline.width})`)
    .toBeGreaterThan(0);
  // outlineColor as RGB: e.g. "rgb(99, 102, 241)" — make sure it's not
  // transparent. Any solid color is fine.
  expect(outline.color).not.toMatch(/rgba?\([^)]*,\s*0\s*\)$/);

  // And: a sibling un-selected section must NOT have the outline.
  const other = page.locator(
    `.chat-preview [data-section-uuid]:not(.selected)`,
  );
  if ((await other.count()) > 0) {
    // Browsers report a default `outline-width: medium` (≈3px) even
    // when `outline-style: none` — the actual line is only drawn when
    // outline-style is something other than none. So we test the
    // style, not the width, when verifying "no outline".
    const otherStyle = await other.first().evaluate((el) => {
      return getComputedStyle(el).outlineStyle;
    });
    expect(
      otherStyle,
      `un-selected message should have outline-style:none (got "${otherStyle}")`,
    ).toBe("none");
  }
});
