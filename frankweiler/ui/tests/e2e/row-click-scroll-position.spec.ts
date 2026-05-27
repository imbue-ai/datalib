import { test, expect } from "@playwright/test";

// Regression: clicking a grid row that maps to a message in the
// *same* conversation that is already loaded in the preview pane must
// scroll the preview pane so the selected message lands at the top
// (and is therefore visible to the user). The existing
// `row-click-scroll.spec.ts` only verifies that the `.selected` class
// moves to the right `[data-section-uuid]` element — but
// `toBeVisible()` is satisfied even when that element is far below
// the scrollport, so it cannot catch a regression in which
// `scrollIntoView` stops firing after the initial conversation load.
//
// What we pin here:
//   1. After clicking row A, the selected message sits near the top
//      of the scrollable `.chat-preview` viewport.
//   2. After clicking row B (different message_index, same conv),
//      the preview pane's scrollTop changes, AND the newly-selected
//      message is what's near the top.
//
// The clearest failure mode this guards against is the second click
// being a no-op visually — exactly what the user reported when
// clicking around inside a single Slack thread.

type Row = {
  uuid: string;
  conversation_uuid: string;
  kind: string;
  message_index: number | null;
};

// Shrink the viewport so the preview pane is forced to be scrollable
// for any conversation with more than a handful of messages — without
// this, on a tall window the entire conversation may fit in the pane
// and `scrollIntoView` is a no-op even when working correctly.
test.use({ viewport: { width: 1024, height: 400 } });

test("clicking same-thread rows scrolls the preview to the new message", async ({
  page,
  request,
}) => {
  const resp = await request.get("/api/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: Row[] };

  // Find a conversation whose message rows span enough indices that
  // the two we pick will live far apart vertically (so a missing
  // scroll is unambiguous, not "off by 8 pixels"). We need the higher
  // of the two messages to be well outside the initial scrollport.
  const byConv = new Map<string, Row[]>();
  for (const r of data.rows) {
    if (r.kind === "Chat") continue;
    if (r.message_index == null) continue;
    const list = byConv.get(r.conversation_uuid) ?? [];
    list.push(r);
    byConv.set(r.conversation_uuid, list);
  }
  // Pick the conversation with the most distinct message rows, then
  // use its first and last messages (by message_index sort order). We
  // need the preview to actually be scrollable
  // (scrollHeight > clientHeight) — checked at runtime below — for the
  // scrollTop assertion to be meaningful.
  let chosen: {
    convUuid: string;
    uuidA: string;
    uuidB: string;
  } | null = null;
  let best = 0;
  for (const [convUuid, list] of byConv) {
    list.sort((a, b) => a.message_index! - b.message_index!);
    const first = list[0];
    const last = list[list.length - 1];
    if (first.uuid === last.uuid) continue;
    if (list.length > best) {
      best = list.length;
      chosen = { convUuid, uuidA: first.uuid, uuidB: last.uuid };
    }
  }
  expect(
    chosen,
    "fixture must contain a conversation with at least two distinct message rows",
  ).not.toBeNull();

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  async function scrollToAndClick(rowUuid: string) {
    const rowIndex = await page.evaluate(
      ({ uuid }) => {
        type Node = {
          rowIndex: number | null;
          data?: { uuid: string };
        };
        const w = window as unknown as {
          __fwGridApi?: {
            forEachNode: (cb: (n: Node) => void) => void;
            ensureNodeVisible: (n: Node, pos: "middle") => void;
          };
        };
        const api = w.__fwGridApi!;
        let foundIdx: number | null = null;
        api.forEachNode((node) => {
          if (node.data && node.data.uuid === uuid) {
            api.ensureNodeVisible(node, "middle");
            foundIdx = node.rowIndex;
          }
        });
        return foundIdx;
      },
      { uuid: rowUuid },
    );
    expect(rowIndex, `node for uuid=${rowUuid} found in grid`).not.toBeNull();
    const row = page.locator(
      `.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`,
    );
    await expect(row).toBeVisible({ timeout: 5_000 });
    await row.click();
  }

  // Returns: pane scrollTop, pane viewport top, and the target
  // section's bounding-rect top. `target.top - pane.top` is how far
  // below the top edge of the scrollport the section is sitting.
  async function geom(sectionUuid: string) {
    return await page.evaluate((uuid) => {
      const pane = document.querySelector(".chat-preview") as HTMLElement | null;
      const target = document.querySelector(
        `[data-section-uuid="${uuid}"]`,
      ) as HTMLElement | null;
      if (!pane || !target) return null;
      const pr = pane.getBoundingClientRect();
      const tr = target.getBoundingClientRect();
      return {
        scrollTop: pane.scrollTop,
        scrollHeight: pane.scrollHeight,
        clientHeight: pane.clientHeight,
        paneTop: pr.top,
        targetTop: tr.top,
        offsetFromPaneTop: tr.top - pr.top,
      };
    }, sectionUuid);
  }

  await scrollToAndClick(chosen!.uuidA);
  // Wait for the chat body to render and selection to apply.
  await expect(
    page.locator(`.chat-preview [data-section-uuid="${chosen!.uuidA}"].selected`),
  ).toBeVisible({ timeout: 10_000 });
  // Allow scrollIntoView to settle.
  await page.waitForTimeout(150);

  const gA = await geom(chosen!.uuidA);
  expect(gA, "geometry for first selection").not.toBeNull();
  // The pane must actually be scrollable for the rest of the test to
  // be meaningful. If the entire conversation fits without scrolling
  // there's no scrollTop change to observe — skip with a clear
  // message rather than silently passing.
  test.skip(
    gA!.scrollHeight <= gA!.clientHeight + 4,
    `pane not scrollable (scrollHeight=${gA!.scrollHeight} ` +
      `clientHeight=${gA!.clientHeight}); pick a taller fixture or ` +
      `shorter viewport`,
  );
  // First-selection sanity: uuidA is the *first* message in the
  // conversation, so applySelection should put it at the very top of
  // the pane (scroll-margin-top adds ~16px). Generous slack for
  // header/padding.
  expect(
    gA!.offsetFromPaneTop,
    `first-click should put msg ${chosen!.uuidA} near the top of .chat-preview, ` +
      `but it was offset by ${gA!.offsetFromPaneTop}px`,
  ).toBeLessThan(120);

  // Now click the second row (same conversation, much later message).
  await scrollToAndClick(chosen!.uuidB);
  await expect(
    page.locator(`.chat-preview [data-section-uuid="${chosen!.uuidB}"].selected`),
  ).toBeVisible({ timeout: 10_000 });
  await page.waitForTimeout(150);

  const gB = await geom(chosen!.uuidB);
  expect(gB, "geometry for second selection").not.toBeNull();

  // The scrollTop *must* have changed — otherwise the user sees the
  // exact same view despite clicking a different row. (The original
  // bug: same-conversation prop changes left scrollTop untouched.)
  expect(
    gB!.scrollTop,
    `clicking msg ${chosen!.uuidB} (after msg ${chosen!.uuidA}) should change ` +
      `.chat-preview scrollTop, but it stayed at ${gA!.scrollTop}`,
  ).not.toBe(gA!.scrollTop);

  // The newly-selected message must be inside the pane's viewport. We
  // can't insist it's at the *top* — for messages near the end of the
  // conversation the pane bottoms out at `scrollHeight - clientHeight`
  // and the message ends up part-way down the viewport. "Visible at
  // all" is the contract the user cares about.
  const tBPos = gB!.offsetFromPaneTop;
  expect(
    tBPos >= 0 && tBPos < gB!.clientHeight,
    `msg ${chosen!.uuidB} should be inside the pane viewport ` +
      `(0..${gB!.clientHeight}), but offsetFromPaneTop=${tBPos}`,
  ).toBeTruthy();
});
