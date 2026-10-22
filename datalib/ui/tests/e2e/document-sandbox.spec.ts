// A document body is drawn in a frame of its own (`src/cards/docFrame.ts`).
// The sanitizer is meant to make the body safe before it gets there;
// this spec checks the layer behind it by writing into a real document's
// frame what a sanitizer miss, or a sender, could put there:
//
// - markup that would run script. The app page's own policy already
//   refuses inline script, so the test that tells the frame apart is the
//   one loading a script from this origin, which that policy allows and
//   the frame's `script-src 'none'` does not;
// - a `position: fixed` cover, which the sanitizer keeps (it keeps
//   `style=`). In the page it would lie over the whole app, a ready-made
//   fake prompt; in the frame it stays inside the frame.
//
// Each payload has a control — an event of the test's own that proves
// the thing which would have run it really happened — so "nothing ran"
// is not "nothing happened yet". It runs in WebKit too, the desktop
// app's engine.

import { test, expect, type Page } from "@playwright/test";

async function openADocument(page: Page): Promise<void> {
  const resp = await page.request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { markdown_uuid: string | null; provider: string }[];
  };
  // An email: the content this frame is most for.
  const pick = rows.find((r) => r.markdown_uuid && r.provider === "email");
  expect(pick, "fixture must have an email").toBeTruthy();
  await page.goto(`/chat/${pick!.markdown_uuid}`);
  // The copy button is the app's decoration, so the body is painted and
  // the app's code has been in the frame.
  await expect(
    page.frameLocator("iframe.doc-frame").locator("button.copy-uuid").first(),
  ).toBeAttached({ timeout: 10_000 });
}

test("a document's frame runs no inline script", async ({ page }) => {
  await openADocument(page);
  const ran = await page.locator("iframe.doc-frame").evaluate(async (el: HTMLIFrameElement) => {
    const doc = el.contentDocument!;
    const ran: string[] = [];
    (window as unknown as { __docFrameRan: string[] }).__docFrameRan = ran;
    const fired: string[] = [];
    const host = doc.createElement("div");
    host.innerHTML =
      `<img id="p-img" src="data:," onerror="parent.__docFrameRan.push('onerror')">` +
      `<details id="p-details" ontoggle="parent.__docFrameRan.push('ontoggle')"></details>`;
    doc.body.append(host);
    const img = doc.getElementById("p-img")!;
    const details = doc.getElementById("p-details") as HTMLDetailsElement;
    const errored = new Promise<void>((resolve) =>
      img.addEventListener("error", () => (fired.push("error"), resolve())),
    );
    const toggled = new Promise<void>((resolve) =>
      details.addEventListener("toggle", () => (fired.push("toggle"), resolve())),
    );
    details.open = true;
    // A script element runs as it is inserted, when its document may run any.
    const script = doc.createElement("script");
    script.textContent = "parent.__docFrameRan.push('script')";
    doc.body.append(script);
    await Promise.all([errored, toggled]);
    return { ran, fired };
  });

  expect(ran.fired.sort(), "the events that would run the payloads fired").toEqual([
    "error",
    "toggle",
  ]);
  expect(ran.ran, "no payload in the frame may run").toEqual([]);
});

test("a document's frame runs no script, not even this origin's", async ({ page }) => {
  await openADocument(page);
  const result = await page.locator("iframe.doc-frame").evaluate(async (el: HTMLIFrameElement) => {
    const doc = el.contentDocument!;
    const script = doc.createElement("script");
    // Any classic script this origin serves would do. DACTAL's engine is
    // one, public and static, and it says it ran by setting `DACTAL`.
    script.src = "/dactal/vendor/dactal.js";
    const settled = new Promise<string>((resolve) => {
      script.addEventListener("load", () => resolve("load"));
      script.addEventListener("error", () => resolve("error"));
    });
    doc.body.append(script);
    const how = await settled;
    const has = (w: Window | null) => typeof (w as unknown as { DACTAL?: unknown })?.DACTAL;
    return { how, inFrame: has(el.contentWindow), inApp: has(window) };
  });
  expect(result).toEqual({ how: "error", inFrame: "undefined", inApp: "undefined" });
});

test("a document cannot cover the app's own controls", async ({ page }) => {
  await openADocument(page);
  await page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement) => {
    const doc = el.contentDocument!;
    const cover = doc.createElement("div");
    cover.id = "p-cover";
    cover.setAttribute("style", "position:fixed;inset:0;z-index:2147483647;background:#fff");
    cover.textContent = "Your session has expired. Enter your password to continue.";
    doc.body.append(cover);
  });
  await expect(page.frameLocator("iframe.doc-frame").locator("#p-cover")).toBeVisible();
  // A trial click checks the box would receive the click, not the cover.
  await page
    .getByRole("searchbox", { name: "Search your data" })
    .click({ trial: true, timeout: 5_000 });
});

test("the app's own controls still work inside the frame", async ({ page }) => {
  await openADocument(page);
  const body = page.frameLocator("iframe.doc-frame").locator("body.chat-body");
  // A listener of the app's, on the frame's document, answers the click:
  // the button reports the copy, or that the clipboard refused it.
  const copy = body.locator("button.copy-uuid").first();
  await copy.click();
  await expect(copy).toHaveClass(/copied|copy-failed/);
});

/** Quark's prize email in the TNG fixture carries every trick the audits found in a body:
 *  a link whose path is card source, a link to the API, a `javascript:` link, a malformed
 *  `datalib:` link, a frame on our own `blobs/` route, and images on API routes that do
 *  work when read (`?refresh=1` walks the data root). Opening it and clicking across it
 *  must run no code from the email, write nothing, and load nothing from the API. */
test("a hostile email's links run nothing and write nothing", async ({ page, context }) => {
  const resp = await page.request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { markdown_uuid: string | null; conversation_name: string }[];
  };
  const prize = rows.find((r) => r.markdown_uuid && /holosuite/i.test(r.conversation_name));
  expect(prize, "the fixture must have Quark's prize email").toBeTruthy();

  const writes: string[] = [];
  const apiLoads: string[] = [];
  context.on("request", (r) => {
    if (r.method() !== "GET" && /\/api\/(config|lib|requests)\b/.test(r.url())) {
      writes.push(`${r.method()} ${r.url()}`);
    }
    // The email's images name `from=holosuite`; the app's own reads of these routes do not.
    const url = new URL(r.url());
    if (url.pathname.startsWith("/api/") && url.searchParams.get("from") === "holosuite") {
      apiLoads.push(url.pathname + url.search);
    }
  });
  const ranIn = (p: typeof page) =>
    p.evaluate(() => (window as unknown as { __hostileRan?: number }).__hostileRan);

  await page.goto(`/chat/${prize!.markdown_uuid}`);
  const body = page.frameLocator("iframe.doc-frame").locator("body.chat-body");
  await body.getByText("Unsubscribe").click();
  await body.getByText("Ask Quark").click();
  await body.getByRole("link", { name: "Review your account" }).click();
  await body.getByRole("link", { name: "Claim your holosuite upgrade" }).click();
  // The control: the claim link does open the app on the path the email chose, and
  // that path reaches the card machinery, which says it does not parse rather than
  // running it. Found by what it shows: the other clicks open windows of their own,
  // in an order the engines do not agree on.
  const refused = async () => {
    for (const p of context.pages()) {
      if (await p.locator(".ct-card", { hasText: "does not parse" }).count()) return true;
    }
    return false;
  };
  await expect.poll(refused, { timeout: 10_000 }).toBe(true);

  for (const p of context.pages()) {
    if (p.url().startsWith("http")) expect(await ranIn(p), p.url()).toBeUndefined();
  }
  expect(writes).toEqual([]);
  expect(apiLoads).toEqual([]);
});
