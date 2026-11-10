// One message of a document cannot reach the next, whatever HTML it holds
// (docs/dev/data_architecture_parse_and_render.md §"One section cannot reach
// the next"). Each test opens a real conversation with a payload written into
// its first message, as the document route would serve a sender's text the
// renderer let through: a `position: fixed` cover, an unclosed
// `<div style="display:none">`, and a copy of the next message's wrapper.
//
// Two guards stand between the payload and the next message: the UI renders
// each message as a fragment of its own, from the sections the route sends,
// and every wrapper has `contain: paint`. The first test has both on; each
// control turns one off and shows the payload then does reach the next
// message, so a green first test is the guards at work.

import { test, expect, type Page } from "@playwright/test";

/// What the document route sends, as far as these tests read it.
type BodySection = { uuid: string | null; len: number };
type ChatResponse = { body: string; sections?: BodySection[] } & Record<string, unknown>;

const COVER = "Your session has expired. Enter your password to continue.";
/// Inside the first message's wrapper, before its closing tag.
const payload = (next: string) =>
  `\n<div id="p-cover" style="position:fixed;inset:0;z-index:2147483647;background:#fff">${COVER}</div>\n\n` +
  `<div style="display:none">\n\n` +
  `<div id="m-${next}" data-section-uuid="${next}" class="msg">\n\nA copy of the next message.\n`;

type Doc = { uuid: string; chat: ChatResponse; first: string; next: string };

/// A conversation the route sends with two messages or more.
async function aConversation(page: Page): Promise<Doc> {
  const resp = await page.request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { markdown_uuid: string | null }[] };
  const uuids = [...new Set(rows.flatMap((r) => (r.markdown_uuid ? [r.markdown_uuid] : [])))];
  for (const uuid of uuids) {
    const got = await page.request.get(`/applet/unified_document_index/chat/${uuid}`);
    if (!got.ok()) continue;
    const chat = (await got.json()) as ChatResponse;
    const pair = twoMessagesInARow(chat);
    if (pair) return { uuid, chat, ...pair };
  }
  throw new Error("the fixture must have a conversation of two messages that comes with sections");
}

/// Two messages side by side, neither inside a fold, so the second is
/// on screen once scrolled to.
function twoMessagesInARow(chat: ChatResponse): { first: string; next: string } | null {
  let at = 0;
  let depth = 0;
  let previous: string | null = null;
  for (const s of chat.sections ?? []) {
    const md = chat.body.slice(at, (at += s.len));
    if (s.uuid === null) {
      if (md.startsWith("<details")) depth += 1;
      if (md.startsWith("</details>")) depth -= 1;
      previous = null;
    } else if (depth === 0 && previous) {
      return { first: previous, next: s.uuid };
    } else {
      previous = depth === 0 ? s.uuid : null;
    }
  }
  return null;
}

/// The document with the payload in its first message, sections adjusted.
function withPayload(doc: Doc): ChatResponse {
  const sections = doc.chat.sections!;
  let at = 0;
  const pieces = sections.map((s) => ({ ...s, md: doc.chat.body.slice(at, (at += s.len)) }));
  const first = pieces.find((p) => p.uuid === doc.first)!;
  const close = first.md.lastIndexOf("</div>");
  expect(close, "the first message is in a wrapper").toBeGreaterThan(0);
  first.md = first.md.slice(0, close) + payload(doc.next) + first.md.slice(close);
  return {
    ...doc.chat,
    body: pieces.map((p) => p.md).join(""),
    sections: pieces.map(({ uuid, md }): BodySection => ({ uuid, len: md.length })),
  };
}

async function open(page: Page, doc: Doc, served: ChatResponse): Promise<void> {
  await page.route(`**/applet/unified_document_index/chat/${doc.uuid}`, (route) =>
    route.fulfill({ json: served }),
  );
  await page.goto(`/chat/${doc.uuid}`);
  await expect(page.frameLocator("iframe.doc-frame").locator("#p-cover")).toBeAttached({
    timeout: 10_000,
  });
}

/// Which message the reader would touch at the middle of the next
/// message's header, once it is scrolled into view.
const touchedAtNext = (page: Page, next: string) =>
  page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement, uuid: string) => {
    const doc = el.contentDocument!;
    const target = doc.querySelectorAll(`[data-section-uuid="${uuid}"]`);
    const genuine = target[target.length - 1] as HTMLElement;
    genuine.scrollIntoView({ block: "center" });
    const r = genuine.getBoundingClientRect();
    const hit = doc.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height / 2, 12));
    return hit?.closest("[data-section-uuid]")?.getAttribute("data-section-uuid") ?? null;
  }, next);

test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

test("a message's payload neither hides, replaces nor covers the next message", async ({
  page,
}) => {
  const doc = await aConversation(page);
  await open(page, doc, withPayload(doc));
  const frame = page.frameLocator("iframe.doc-frame");
  // Only the renderer's wrapper answers to the next message's id, and it shows.
  await expect(frame.locator(`[data-section-uuid="${doc.next}"]`)).toHaveCount(1);
  await expect(frame.locator(`[id="m-${doc.next}"]`)).toBeVisible();
  // The cover stays inside the first message's box.
  await expect.poll(() => touchedAtNext(page, doc.next)).toBe(doc.next);
});

test("control: served without its sections, the payload hides the next message", async ({
  page,
}) => {
  const doc = await aConversation(page);
  const { sections: _, ...whole } = withPayload(doc);
  await open(page, doc, whole);
  const frame = page.frameLocator("iframe.doc-frame");
  await expect(frame.locator(`[data-section-uuid="${doc.next}"]`)).toHaveCount(2);
  await expect(frame.locator(`[data-section-uuid="${doc.next}"]`).last()).toBeHidden();
});

test("control: without `contain: paint`, the cover lies over the next message", async ({
  page,
}) => {
  const doc = await aConversation(page);
  await open(page, doc, withPayload(doc));
  await page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement) => {
    for (const m of el.contentDocument!.querySelectorAll<HTMLElement>(".msg")) {
      m.style.contain = "none";
    }
  });
  await expect.poll(() => touchedAtNext(page, doc.next)).toBe(doc.first);
});
