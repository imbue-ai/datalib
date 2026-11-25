// A long document is read a page at a time
// (docs/dev/plans/completed/document_viewer_from_blocks.md §"Step 6"): it opens with
// a page and draws the next as the reader nears an end, above or below,
// until every message is there once and in the document's order. The TNG
// documents are short, so every page asked for here is cut to two nodes on
// its way out.

import { test, expect, type Page } from "@playwright/test";

type BodyNode = {
  uuid?: string | null;
  class?: string | null;
  fold?: string;
  pieces?: BodyNode[];
};
type ChatResponse = { pieces: BodyNode[]; before?: number | null; after?: number | null };

const chatRoute = (uuid: string) => `/applet/unified_grid_and_search_index/chat/${uuid}`;

/// The messages at the top level of a document, in its order.
const topMessages = (nodes: BodyNode[]): string[] =>
  nodes.flatMap((n) => (n.fold === undefined && n.uuid && n.class ? [n.uuid] : []));

/// A document with at least eight top-level messages, and their order.
async function aLongDocument(page: Page): Promise<{ uuid: string; order: string[] }> {
  const resp = await page.request.get("/applet/unified_grid_and_search_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: { markdown_uuid: string | null }[] };
  const uuids = [...new Set(rows.flatMap((r) => (r.markdown_uuid ? [r.markdown_uuid] : [])))];
  for (const uuid of uuids) {
    const got = await page.request.get(`${chatRoute(uuid)}?pieces=true`);
    if (!got.ok()) continue;
    const order = topMessages(((await got.json()) as ChatResponse).pieces);
    if (order.length >= 8) return { uuid, order };
  }
  throw new Error("the fixture must have a document of eight top-level messages");
}

/// Every page the viewer asks for comes two nodes long; the answers are
/// kept to read.
async function smallPages(page: Page, uuid: string): Promise<ChatResponse[]> {
  const answers: ChatResponse[] = [];
  await page.route(
    (url) => url.pathname === chatRoute(uuid) && url.searchParams.has("limit"),
    async (route) => {
      const url = new URL(route.request().url());
      url.searchParams.set("limit", "2");
      const resp = await route.fetch({ url: url.toString() });
      const json = (await resp.json()) as ChatResponse;
      answers.push(json);
      await route.fulfill({ response: resp, json });
    },
  );
  return answers;
}

/// The messages drawn at the top level of the frame, in order.
const drawn = (page: Page) =>
  page
    .locator("iframe.doc-frame")
    .evaluate((el: HTMLIFrameElement) =>
      [...el.contentDocument!.body.querySelectorAll(":scope > [data-section-uuid]")].map((e) =>
        e.getAttribute("data-section-uuid"),
      ),
    );

const scrollFrame = (page: Page, to: "top" | "bottom") =>
  page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement, to: string) => {
    const s = el.contentDocument!.scrollingElement!;
    s.scrollTop = to === "top" ? 0 : s.scrollHeight;
  }, to);

test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

test("a long document opens with a page and draws the rest as the reader scrolls down", async ({
  page,
}) => {
  const doc = await aLongDocument(page);
  const answers = await smallPages(page, doc.uuid);
  await page.goto(`/chat/${doc.uuid}`);
  await expect(
    page.frameLocator("iframe.doc-frame").locator(`[data-section-uuid="${doc.order[0]}"]`),
  ).toBeAttached({ timeout: 10_000 });
  expect(answers[0]?.after, "the first answer is a page, not the document").not.toBeNull();
  await expect
    .poll(
      async () => {
        await scrollFrame(page, "bottom");
        return drawn(page);
      },
      { timeout: 20_000 },
    )
    .toEqual(doc.order);
  expect(answers.length, "the rest came a page at a time").toBeGreaterThan(2);
});

test("a document opened at its last message draws what is above as the reader scrolls up", async ({
  page,
}) => {
  const doc = await aLongDocument(page);
  const last = doc.order[doc.order.length - 1]!;
  const answers = await smallPages(page, doc.uuid);
  const card = `documentView(${JSON.stringify(doc.uuid)}, ${JSON.stringify(last)})`;
  await page.goto(`/${encodeURIComponent(card)}`);
  const frame = page.frameLocator("iframe.doc-frame");
  await expect(frame.locator(`[data-section-uuid="${last}"].selected`)).toBeAttached({
    timeout: 10_000,
  });
  expect(answers[0]?.before, "it opens on the page around the message").not.toBeNull();
  await expect
    .poll(
      async () => {
        await scrollFrame(page, "top");
        return drawn(page);
      },
      { timeout: 20_000 },
    )
    .toEqual(doc.order);
});
