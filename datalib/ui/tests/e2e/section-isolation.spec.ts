// One message of a document cannot reach the next, whatever HTML it holds
// (docs/dev/data_architecture_parse_and_render.md §"One section cannot reach
// the next"). Each test opens a real conversation with a payload written into
// its first message, as the document route would serve a sender's text the
// renderer let through: a `position: fixed` cover, an unclosed
// `<div style="display:none">`, and a copy of the next message's wrapper.
//
// Two guards stand between the payload and the next message: the UI renders
// each message as a fragment of its own, in a box it draws, from the pieces
// the route sends, and every box has `contain: paint`. The first test has both on; each
// control turns one off and shows the payload then does reach the next
// message, so a green first test is the guards at work.

import { test, expect, type Page } from "@playwright/test";

/// What the document route sends the viewer, as far as these tests read it.
type BodyPiece = { uuid: string | null; class?: string | null; md: string };
type BodyFold = { fold: string; summary: string; pieces: BodyNode[] };
type BodyNode = BodyPiece | BodyFold;
type ChatResponse = { pieces: BodyNode[] } & Record<string, unknown>;

const isFold = (n: BodyNode): n is BodyFold => "fold" in n;

const chatRoute = (uuid: string) => `/applet/unified_grid_and_search_index/chat/${uuid}`;

const COVER = "Your session has expired. Enter your password to continue.";
/// At the end of what is inside the first message's box.
const payload = (next: string) =>
  `\n<div id="p-cover" style="position:fixed;inset:0;z-index:2147483647;background:#fff">${COVER}</div>\n\n` +
  `<div style="display:none">\n\n` +
  `<div id="m-${next}" data-section-uuid="${next}" class="msg">\n\nA copy of the next message.\n`;

type Doc = { uuid: string; chat: ChatResponse; first: string; next: string };

/// A conversation the route sends with two messages or more.
async function aConversation(page: Page): Promise<Doc> {
  const resp = await page.request.get("/applet/unified_grid_and_search_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { markdown_uuid: string | null }[] };
  const uuids = [...new Set(rows.flatMap((r) => (r.markdown_uuid ? [r.markdown_uuid] : [])))];
  for (const uuid of uuids) {
    const got = await page.request.get(`${chatRoute(uuid)}?pieces=true`);
    if (!got.ok()) continue;
    const chat = (await got.json()) as ChatResponse;
    const pair = twoMessagesInARow(chat);
    if (pair) return { uuid, chat, ...pair };
  }
  throw new Error("the fixture must have a conversation of two messages side by side");
}

/// Two messages side by side, neither inside a fold, so the second is
/// on screen once scrolled to.
function twoMessagesInARow(chat: ChatResponse): { first: string; next: string } | null {
  let previous: string | null = null;
  for (const n of chat.pieces) {
    if (isFold(n) || n.uuid === null) {
      previous = null;
    } else if (previous) {
      return { first: previous, next: n.uuid };
    } else {
      previous = n.uuid;
    }
  }
  return null;
}

/// The document with the payload at the end of its first message.
function withPayload(doc: Doc): ChatResponse {
  const pieces = doc.chat.pieces.map((p) => ({ ...p }));
  const first = pieces.find((p): p is BodyPiece => !isFold(p) && p.uuid === doc.first)!;
  expect(first.class, "the first message is in a box").toBeTruthy();
  first.md += payload(doc.next);
  return { ...doc.chat, pieces };
}

/// The pieces as one string, each box and fold drawn as render draws it.
const joined = (pieces: BodyNode[]): string =>
  pieces
    .map((p) =>
      isFold(p)
        ? `<details class="${p.fold}">\n<summary>${p.summary}</summary>\n\n${joined(p.pieces)}</details>\n\n`
        : p.uuid && p.class
          ? `<div id="m-${p.uuid}" data-section-uuid="${p.uuid}" class="${p.class}">\n\n${p.md}</div>\n\n`
          : p.md,
    )
    .join("");

async function open(page: Page, doc: Doc, served: ChatResponse): Promise<void> {
  await page.route(
    (url) => url.pathname === chatRoute(doc.uuid),
    (route) => route.fulfill({ json: served }),
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
  // The next message's box is where the viewer drew it, and it shows; the
  // copy of its wrapper is still there, inside the first message's box,
  // where nothing that looks a message up by its id finds it first.
  await expect(frame.locator(`body > [data-section-uuid="${doc.next}"]`)).toBeVisible();
  await expect(
    frame.locator(`body > [data-section-uuid="${doc.first}"] [data-section-uuid="${doc.next}"]`),
  ).toHaveCount(1);
  // The cover stays inside the first message's box.
  await expect.poll(() => touchedAtNext(page, doc.next)).toBe(doc.next);
});

test("control: served as one piece, the payload hides the next message", async ({ page }) => {
  const doc = await aConversation(page);
  const served = withPayload(doc);
  const whole = { uuid: null, md: joined(served.pieces) };
  await open(page, doc, { ...served, pieces: [whole] });
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
