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
  const resp = await page.request.get("/applet/unified_index/search?q=&limit=2000");
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
