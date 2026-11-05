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

import { test, expect, type BrowserContext, type Frame, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";

declare const process: { env: Record<string, string | undefined> };

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

// A link need not be an `<a>`: an image map's `<area>` and SVG's `<a>` are links
// too, and with `target=_top` one takes the whole app's tab. Two guards stand
// between a body's and a click: the sanitizer drops the `<area>` and any target
// but `_blank` (`sanitize.test.ts` holds it to that), and the frame cancels
// every click the app did not take itself (`finishFrameClick`). These write the
// links in past the sanitizer; the second turns the frame's guard off too, and
// checks each link then really takes the tab.

const OTHER_LINKS =
  `<map name="p-map"><area id="p-area" shape="rect" coords="0,0,200,40" href="/?hijacked=area" target="_top"></map>` +
  `<img id="p-map-img" usemap="#p-map" width="200" height="40" style="display:block;width:200px;height:40px;background:#08f" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7">` +
  `<svg id="p-svg" width="200" height="40" style="display:block;margin-top:20px">` +
  `<a xlink:href="/?hijacked=svg" target="_top"><rect width="200" height="40" fill="#f80"/></a></svg>`;

/** Where each link is clicked — the image its map covers, the SVG link's
 *  shape — and the element the click lands on. */
const OTHER_LINK_SPOTS = [
  { name: "area", at: "#p-map-img", target: "p-area" },
  { name: "svg", at: "#p-svg rect", target: "rect" },
];

async function writeOtherLinks(page: Page): Promise<void> {
  await openADocument(page);
  await page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement, html: string) => {
    const host = el.contentDocument!.createElement("div");
    host.innerHTML = html;
    el.contentDocument!.body.prepend(host);
    // The control: the test's own listener, last on the frame's window, sees
    // each click after every listener of the app's.
    const seen: string[] = [];
    (window as unknown as { __clicks: string[] }).__clicks = seen;
    el.contentWindow!.addEventListener("click", (ev) => {
      const t = ev.target as Element;
      seen.push(`${t.id || t.localName}: ${ev.defaultPrevented ? "cancelled" : "followed"}`);
    });
  }, OTHER_LINKS);
}

/** The clicks the frame saw; null once a link has taken the tab, with the page that held them. */
const clicksSeen = (page: Page) =>
  page.evaluate(() => {
    const seen = (window as unknown as { __clicks?: string[] }).__clicks;
    return seen ? [...seen] : null;
  });

test("a link of another shape in a body cannot take the app's tab", async ({ page }) => {
  await writeOtherLinks(page);
  const before = page.url();
  const frame = page.frameLocator("iframe.doc-frame");
  const expected: string[] = [];
  for (const { at, target } of OTHER_LINK_SPOTS) {
    await frame.locator(at).click({ position: { x: 10, y: 10 } });
    expected.push(`${target}: cancelled`);
    await expect.poll(() => clicksSeen(page)).toEqual(expected);
  }
  expect(page.url()).toBe(before);
});

test.describe("with the frame's click guard turned off", () => {
  for (const { name, at } of OTHER_LINK_SPOTS) {
    test(`the ${name} link takes the app's tab`, async ({ page }) => {
      await writeOtherLinks(page);
      // The guard is the app's listener on the frame's document; a capturing
      // listener on the frame's window runs first and keeps every click from it.
      await page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement) => {
        el.contentWindow!.addEventListener("click", (ev) => ev.stopImmediatePropagation(), true);
      });
      await page
        .frameLocator("iframe.doc-frame")
        .locator(at)
        .click({ position: { x: 10, y: 10 } });
      await page.waitForURL(new RegExp(`hijacked=${name}`), { timeout: 10_000 });
    });
  }
});

// Quark's prize email in the TNG fixture carries every trick the audits found in a
// body. Between its sender and a click stand five guards:
//
// 1. the email renderer: htmd turns the HTML into markdown, which keeps only a
//    link's href and an image's src, and drops the iframe; markdown-it then
//    refuses a `javascript:` or `data:` link and escapes a stray `%`;
// 2. the sanitizer (`src/cards/sanitize.ts`);
// 3. the policies: the frame's `script-src 'none'; frame-src 'none'`, and the
//    app page's own;
// 4. the server: `frame-ancestors 'none'` on every page it serves, and a GET
//    that refuses `?refresh=` rather than walking the data root;
// 5. the app: a link in the frame never navigates it (the app opens it with
//    `noopener`), and a card's source is parsed, never run.
//
// The first test opens the email as a person would. Each test after it writes the
// email in as its sender wrote it, past one more guard, and says what the next
// one catches; the last turns off 1 to 3 and the server's `frame-ancestors`, and
// checks the tricks then really work. So a trick that stops working shows up as a
// red last test, not as a green first one. The server's refusal of `?refresh=` stays
// on (`http_tests` checks it), so those images still get their 400.

/** The email's HTML as its sender wrote it, from the mailbox the fixture ingests. */
function quarksHtml(): string {
  const mbox = readFileSync(process.env.DATALIB_TEST_E2E_EMAIL_MBOX!, "utf8");
  const headers = mbox.indexOf("Message-Id: <holosuite-win-001@quarks.example>");
  expect(headers, "the mailbox must have Quark's prize email").toBeGreaterThan(-1);
  const start = mbox.indexOf("\n\n", headers) + 2;
  const end = mbox.indexOf("\nFrom ", start);
  return mbox.slice(start, end < 0 ? undefined : end);
}

async function quarksUuid(page: Page): Promise<string> {
  const resp = await page.request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { markdown_uuid: string | null; conversation_name: string }[];
  };
  const prize = rows.find((r) => r.markdown_uuid && /holosuite/i.test(r.conversation_name));
  expect(prize, "the fixture must have Quark's prize email").toBeTruthy();
  return prize!.markdown_uuid!;
}

/** What the email managed to do, read from every window and frame of the context. */
type Seen = {
  /** Each `__hostileRan` a payload set, as `<frame>: <value>`. */
  ran: string[];
  /** The email's images that reached the API, as `<status> <path>`. */
  apiImages: string[];
  /** Anything that wrote to the config, a library or the request queue. */
  writes: string[];
  /** Whether the email's iframe got a document of ours. */
  framed: boolean;
  /** Uncaught errors in any page. */
  errors: string[];
};

function watch(context: BrowserContext) {
  const apiImages: string[] = [];
  const writes: string[] = [];
  const errors: string[] = [];
  const onPage = (p: Page) => p.on("pageerror", (e) => errors.push(`${p.url()}: ${e.message}`));
  context.pages().forEach(onPage);
  context.on("page", onPage);
  context.on("request", (r) => {
    if (r.method() !== "GET" && /\/api\/(config|lib|requests)\b/.test(r.url())) {
      writes.push(`${r.method()} ${r.url()}`);
    }
  });
  // The email's images name `from=holosuite`; the app's own reads of these routes do not.
  context.on("response", (r) => {
    const url = new URL(r.url());
    if (url.pathname.startsWith("/api/") && url.searchParams.get("from") === "holosuite") {
      apiImages.push(`${r.status()} ${url.pathname}`);
    }
  });
  return {
    apiImages: () => [...apiImages].sort(),
    seen: async (): Promise<Seen> => ({
      ran: await ranAnywhere(context),
      apiImages: [...apiImages].sort(),
      writes,
      framed: await framedAnywhere(context),
      errors,
    }),
  };
}

/** A frame the policy refused never gets a document, and WebKit then never
 *  answers an evaluate in it; such a frame has nothing to report. */
function inFrame<T>(f: Frame, fn: () => T): Promise<T | undefined> {
  const none = new Promise<undefined>((resolve) => setTimeout(() => resolve(undefined), 2_000));
  return Promise.race([f.evaluate(fn).catch(() => undefined), none]);
}

async function ranAnywhere(context: BrowserContext): Promise<string[]> {
  const ran: string[] = [];
  for (const p of context.pages()) {
    for (const f of p.frames()) {
      const v = await inFrame(
        f,
        () => (window as unknown as { __hostileRan?: unknown }).__hostileRan,
      );
      if (v !== undefined)
        ran.push(`${f.url().startsWith("http") ? new URL(f.url()).pathname : f.url()}: ${v}`);
    }
  }
  return ran;
}

/** A page of ours framed inside a document's frame. The app boots in it and moves
 *  its URL off `blobs/prize.html`, so it is found by where it sits; a refused frame is
 *  Chromium's `chrome-error:` page or, in WebKit, one with no document. */
async function framedAnywhere(context: BrowserContext): Promise<boolean> {
  for (const p of context.pages()) {
    for (const f of p.frames()) {
      if (f.parentFrame()?.url() !== "about:srcdoc" || !f.url().startsWith("http")) continue;
      if (await inFrame(f, () => document.body.childElementCount > 0)) return true;
    }
  }
  return false;
}

/** Writes the email into the open document's frame, past the renderer and the sanitizer. */
async function writeIntoFrame(page: Page, html: string): Promise<void> {
  await expect(
    page.frameLocator("iframe.doc-frame").locator("button.copy-uuid").first(),
  ).toBeAttached({ timeout: 10_000 });
  await page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement, raw: string) => {
    const host = el.contentDocument!.createElement("div");
    host.id = "as-sent";
    host.innerHTML = raw;
    el.contentDocument!.body.append(host);
  }, html);
}

/** The email's tricks as the frame holds them: the copy written in, when there is one. */
async function whatIsInFrame(page: Page): Promise<Record<string, string | null>> {
  return page.locator("iframe.doc-frame").evaluate((el: HTMLIFrameElement) => {
    const doc = el.contentDocument!;
    const root = doc.getElementById("as-sent") ?? doc.body;
    // A link markdown-it refused is left as its source text, in a paragraph.
    const leaf = (text: string) =>
      Array.from(root.querySelectorAll("a")).find((e) => e.textContent?.trim() === text) ??
      Array.from(root.querySelectorAll("p")).find((e) => e.textContent?.includes(text));
    const href = (text: string) => {
      const e = leaf(text);
      return e?.tagName === "A" ? e.getAttribute("href") : (e?.tagName.toLowerCase() ?? null);
    };
    return {
      unsubscribe: href("Unsubscribe"),
      askQuark: href("Ask Quark"),
      claimStyle: leaf("Claim your holosuite upgrade")?.getAttribute("style") ?? null,
      iframes: String(root.querySelectorAll("iframe").length),
      handlers: Array.from(root.querySelectorAll("*"))
        .flatMap((e) => e.getAttributeNames().filter((a) => a.startsWith("on")))
        .join(" "),
    };
  });
}

/** Clicks every link in the email, then waits for the one click whose effect shows:
 *  the claim link opens the app on the path the email chose, and the card machinery
 *  says that path does not parse rather than running it. The other clicks open
 *  windows of their own, in an order the engines do not agree on. */
async function clickEverything(page: Page, context: BrowserContext): Promise<void> {
  const body = page.frameLocator("iframe.doc-frame").locator("body.chat-body");
  const apiTab = context.waitForEvent(
    "response",
    (r) => new URL(r.url()).pathname === "/api/config" && r.request().isNavigationRequest(),
  );
  for (const text of ["Unsubscribe", "Ask Quark", "Review your account"]) {
    await body.getByText(text).last().click();
  }
  await body.getByText("Claim your holosuite upgrade").last().click();
  const refused = async () => {
    for (const p of context.pages()) {
      if (await p.locator(".ct-card", { hasText: "does not parse" }).count()) return true;
    }
    return false;
  };
  await expect.poll(refused, { timeout: 10_000 }).toBe(true);
  // "Review your account" is a plain GET of the API, which the app opens in a tab of its
  // own. It is a same-origin link, so the rule against a link from another site lets it by.
  await expect
    .poll(() =>
      context.pages().some((p) => new URL(p.url(), "http://x").pathname === "/api/config"),
    )
    .toBe(true);
  const opened = await apiTab;
  expect((await opened.request().allHeaders())["sec-fetch-site"]).toBe("same-origin");
  expect(opened.status()).toBe(200);
}

test.describe("Quark's prize email", () => {
  test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

  test("opened as a person would, it runs nothing and loads nothing from the API", async ({
    page,
    context,
  }) => {
    const w = watch(context);
    await page.goto(`/chat/${await quarksUuid(page)}`);
    await expect(
      page.frameLocator("iframe.doc-frame").locator("button.copy-uuid").first(),
    ).toBeAttached({ timeout: 10_000 });
    // The renderer has neutralised three tricks before the app sees them.
    expect(await whatIsInFrame(page)).toEqual({
      unsubscribe: "p", // markdown-it refused the `javascript:` link: it is text
      askQuark: "datalib:person/%25%25%25", // the stray `%` escaped
      claimStyle: null, // htmd keeps only the href
      iframes: "0", // htmd dropped the iframe
      handlers: "", // and the image's onerror
    });
    await clickEverything(page, context);
    expect(await w.seen()).toEqual({
      ran: [],
      apiImages: [],
      writes: [],
      framed: false,
      errors: [],
    });
  });

  test("past the renderer, the sanitizer stops every trick", async ({ page, context }) => {
    const w = watch(context);
    const uuid = await quarksUuid(page);
    const asSent = quarksHtml();
    await page.route(`**/applet/unified_document_index/chat/${uuid}`, async (route) => {
      const resp = await route.fetch();
      await route.fulfill({ response: resp, json: { ...(await resp.json()), body: asSent } });
    });
    await page.goto(`/chat/${uuid}`);
    await expect(
      page.frameLocator("iframe.doc-frame").getByText("Claim your holosuite upgrade"),
    ).toBeVisible({ timeout: 10_000 });
    expect(await whatIsInFrame(page)).toEqual({
      unsubscribe: null, // the `javascript:` href taken off
      askQuark: "datalib:person/%%%", // the malformed link reaches the app: `errors` below
      // so the renderer was skipped; the style as the browser's CSS parser writes it back
      claimStyle: "display: block; padding: 12px; background: rgb(255, 204, 0);",
      iframes: "0", // every iframe dropped
      handlers: "", // and every handler
    });
    await clickEverything(page, context);
    expect(await w.seen()).toEqual({
      ran: [],
      apiImages: [],
      writes: [],
      framed: false,
      errors: [],
    });
  });

  test("past the sanitizer too, the policies stop script and frames, and the server the walk", async ({
    page,
    context,
  }) => {
    const w = watch(context);
    await page.goto(`/chat/${await quarksUuid(page)}`);
    await writeIntoFrame(page, quarksHtml());
    expect(await whatIsInFrame(page)).toEqual({
      unsubscribe: "javascript:window.__hostileRan='unsubscribe'",
      askQuark: "datalib:person/%%%",
      claimStyle: "display:block;padding:12px;background:#fc0",
      iframes: "1",
      handlers: "onerror",
    });
    // No policy covers an image on our own origin: only the sanitizer keeps these from
    // reaching the API with the person's session. The server answers, and refuses the walk.
    await expect.poll(w.apiImages).toEqual(["400 /api/manage/rows", "400 /api/pipeline/storage"]);
    await clickEverything(page, context);
    expect(await w.seen()).toEqual({
      ran: [],
      apiImages: ["400 /api/manage/rows", "400 /api/pipeline/storage"],
      writes: [],
      framed: false,
      errors: [],
    });
  });

  test.describe("with every guard a test can reach turned off", () => {
    test.use({ bypassCSP: true });

    test("the tricks work, but for the app's own two", async ({ page, context }) => {
      const w = watch(context);
      // The server's `frame-ancestors 'none'` would still refuse the email's iframe.
      await page.route("**/blobs/prize.html", async (route) => {
        const resp = await route.fetch();
        const headers = { ...resp.headers() };
        delete headers["content-security-policy"];
        delete headers["x-frame-options"];
        await route.fulfill({ response: resp, headers });
      });
      await page.goto(`/chat/${await quarksUuid(page)}`);
      await writeIntoFrame(page, quarksHtml());
      // Script in the body runs, in a frame of our origin that holds the session.
      await expect.poll(() => ranAnywhere(context)).toEqual(["about:srcdoc: onerror"]);
      // A page of ours loads inside the document, where it could pose as the app.
      await expect.poll(() => framedAnywhere(context), { timeout: 10_000 }).toBe(true);
      await expect.poll(w.apiImages).toEqual(["400 /api/manage/rows", "400 /api/pipeline/storage"]);
      await clickEverything(page, context);
      // Guard 5 is the app's and stays on: the `javascript:` link still runs nowhere,
      // because the app opens every link with `noopener` instead of following it, and
      // the claim link's card source is parsed, not run.
      expect(await w.seen()).toEqual({
        ran: ["about:srcdoc: onerror"],
        apiImages: ["400 /api/manage/rows", "400 /api/pipeline/storage"],
        writes: [],
        framed: true,
        errors: [],
      });
    });
  });
});

// Quark's bar tab is an attachment typed `text/xsl`. Browsers render that type
// as a page, and a server that sandboxed only the types it knew to be
// dangerous served it with none, in the app's origin and with the session.
// Two guards stand between it and a click now: the server names a type it
// does not know to be shown as bytes, and it serves every attachment under
// the sandbox policy. Each test turns one more off.

async function barTabHref(page: Page): Promise<string> {
  const resp = await page.request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as {
    rows: { markdown_uuid: string | null; conversation_name: string }[];
  };
  const tab = rows.find((r) => r.markdown_uuid && /bar tab/i.test(r.conversation_name));
  expect(tab, "the fixture must have Quark's bar tab").toBeTruthy();
  await page.goto(`/chat/${tab!.markdown_uuid}`);
  const link = page.frameLocator("iframe.doc-frame").getByRole("link", { name: "bar_tab.xsl" });
  await expect(link).toBeAttached({ timeout: 10_000 });
  const href = await link.getAttribute("href");
  expect(href).toMatch(/^\/data\/.+\/part\/[0-9a-f]{64}$/);
  return href!;
}

/** Serves the attachment under the type its sender gave it, and without the
 *  sandbox too unless `sandboxed`. */
async function asItsSenderTypedIt(page: Page, href: string, sandboxed: boolean): Promise<void> {
  await page.route(`**${href}`, async (route) => {
    const resp = await route.fetch();
    const headers: Record<string, string> = {
      ...resp.headers(),
      "content-type": "text/xsl; charset=utf-8",
    };
    if (!sandboxed) delete headers["content-security-policy"];
    await route.fulfill({ response: resp, headers });
  });
}

const hostileRan = (page: Page) =>
  page.evaluate(() => (window as unknown as { __hostileRan?: unknown }).__hostileRan ?? null);

test.describe("Quark's bar tab", () => {
  test.afterEach(({ page }) => page.unrouteAll({ behavior: "ignoreErrors" }));

  test("is served as bytes, in the sandbox", async ({ page }) => {
    const resp = await page.request.get(await barTabHref(page));
    expect(resp.status()).toBe(200);
    expect(await resp.text(), "the bytes are Quark's").toContain("__hostileRan");
    expect(resp.headers()["content-type"]).toBe("application/octet-stream");
    expect(resp.headers()["x-content-type-options"]).toBe("nosniff");
    expect(resp.headers()["content-security-policy"]).toMatch(/^sandbox;/);
  });

  test("typed as Quark typed it, the sandbox still runs nothing", async ({ page }) => {
    const href = await barTabHref(page);
    await asItsSenderTypedIt(page, href, true);
    await page.goto(href);
    // The control: the browser took the bytes for a page.
    await expect(page.locator("h1")).toHaveText(/gold-pressed latinum/);
    expect(await hostileRan(page)).toBeNull();
  });

  test.describe("with both guards turned off", () => {
    test.use({ bypassCSP: true });

    test("the tab runs its script in the app's origin", async ({ page }) => {
      const href = await barTabHref(page);
      await asItsSenderTypedIt(page, href, false);
      await page.goto(href);
      await expect.poll(() => hostileRan(page)).toBe("bar_tab");
    });
  });
});
