import { test, expect } from "@playwright/test";
import { EVERY_ROW, clickRowByUuid, inDocFrame } from "./grid-helpers";

// Clicking a grid row opens that row's document as a column on the
// right with the corresponding section highlighted and scrolled into
// the visible part of the pane. Clicking two grid rows that map to
// *different* messages of the same conversation must highlight
// different sections — and each must actually be on screen, not just
// carry the `.selected` class somewhere below the scrollport.

type Row = {
  uuid: string;
  conversation_uuid: string;
  kind: string;
  message_index: number | null;
};

// Shrink the viewport so the document pane is forced to be scrollable
// for any conversation with more than a handful of messages — without
// this, on a tall window the entire conversation may fit in the pane
// and the scroll behavior goes unexercised.
test.use({ viewport: { width: 1500, height: 450 } });

async function assertSelectedVisible(page: import("@playwright/test").Page, uuid: string) {
  const selected = await inDocFrame(page, `[data-section-uuid="${uuid}"].selected`);
  await expect(selected).toBeVisible({ timeout: 10_000 });
  // The document frame is the pane: its viewport is what the reader sees.
  const inView = await selected.evaluate((el) => {
    const r = el.getBoundingClientRect();
    return r.top >= -1 && r.top < el.ownerDocument.defaultView!.innerHeight;
  });
  expect(inView, `section ${uuid} must be inside the pane viewport`).toBe(true);
}

test("row clicks highlight and scroll to the right message", async ({ page, request }) => {
  // Use the backend search API to find a conversation with two message
  // rows far apart, so the scroll between them is unambiguous.
  const resp = await request.get("/applet/unified_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: Row[] };
  const byConv = new Map<string, Row[]>();
  for (const r of data.rows) {
    // Providers stamp chat-level rows with their own prefix ("Signal
    // Chat", "WhatsApp Chat", "Beeper:Signal Chat"); accept any suffix
    // ending in "Chat" / "Thread" / "Reaction" so we only walk
    // per-message rows here. (Previously `=== "Chat"` was used, but it
    // missed `WhatsApp Chat`; keep the broader pattern even now that
    // the backend `null → 0` read-path bug is fixed, since the
    // taxonomy is genuinely prefixed.)
    if (/(Chat|Thread|Reaction)$/.test(r.kind)) continue;
    if (r.message_index == null) continue;
    const list = byConv.get(r.conversation_uuid) ?? [];
    list.push(r);
    byConv.set(r.conversation_uuid, list);
  }
  // Pick the conversation with the most message rows; use its first
  // and last (by message_index).
  let chosen: { uuidA: string; uuidB: string } | null = null;
  let best = 0;
  for (const list of byConv.values()) {
    list.sort((a, b) => a.message_index! - b.message_index!);
    const first = list[0];
    const last = list[list.length - 1];
    if (first.uuid === last.uuid) continue;
    if (list.length > best) {
      best = list.length;
      chosen = { uuidA: first.uuid, uuidB: last.uuid };
    }
  }
  expect(
    chosen,
    "fixture must contain a conversation with at least two distinct message rows",
  ).not.toBeNull();

  await page.goto(EVERY_ROW);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });

  await clickRowByUuid(page, chosen!.uuidA);
  await assertSelectedVisible(page, chosen!.uuidA);

  await clickRowByUuid(page, chosen!.uuidB);
  await assertSelectedVisible(page, chosen!.uuidB);
  // The previous selection is gone — exactly one selected section.
  // The previous selection is gone from every document frame.
  await expect
    .poll(async () => {
      let n = 0;
      for (const f of page.frames()) {
        n += await f
          .locator(`[data-section-uuid="${chosen!.uuidA}"].selected`)
          .count()
          .catch(() => 0);
      }
      return n;
    })
    .toBe(0);
});
