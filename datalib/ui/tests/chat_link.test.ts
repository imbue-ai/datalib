import { describe, it, expect } from "vitest";
import {
  chatSectionFromHref,
  chatUuidFromHref,
  finishFrameClick,
  linkFromClick,
  rewriteIdButtonsForCopy,
} from "../src/cards/chatLink";
import { copyWithHandles } from "../src/cards/contacts";

// Forge a MouseEvent-shaped object whose `target` is an `<a>` carrying
// the given href (or an arbitrary descendant of it). jsdom's
// `MouseEvent` doesn't let you assign `target` after construction, so
// we cast a plain object literal instead — `linkFromClick` only
// reads `target`, modifier-key fields, and `button`.
type FakeMouseEvent = {
  target: Element | null;
  metaKey?: boolean;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  button?: number;
};

function clickOn(
  href: string | null,
  opts: Partial<FakeMouseEvent> = {},
  nest = false,
): MouseEvent {
  const a = document.createElement("a");
  if (href !== null) a.setAttribute("href", href);
  let target: Element = a;
  if (nest) {
    // Click on a child element — `closest("a")` should still find the
    // ancestor `<a>`.
    const span = document.createElement("span");
    a.appendChild(span);
    target = span;
  }
  return {
    target,
    metaKey: false,
    ctrlKey: false,
    shiftKey: false,
    button: 0,
    ...opts,
  } as unknown as MouseEvent;
}

describe("linkFromClick", () => {
  it("names the link as written, resolved, and how it was clicked", () => {
    const link = linkFromClick(clickOn("/chat/abc-123"));
    expect(link).toEqual({
      href: "/chat/abc-123",
      resolved: new URL("/chat/abc-123", document.baseURI).href,
      browserClick: false,
    });
    expect(chatUuidFromHref(link!.href)).toBe("abc-123");
  });

  it("walks up to find an ancestor <a>", () => {
    expect(linkFromClick(clickOn("/chat/u2", {}, true))?.href).toBe("/chat/u2");
  });

  it("is null when the click isn't on a link with an href", () => {
    const div = document.createElement("div");
    expect(linkFromClick({ target: div, button: 0 } as unknown as MouseEvent)).toBeNull();
    expect(linkFromClick(clickOn(null))).toBeNull();
    expect(linkFromClick({ target: null } as unknown as MouseEvent)).toBeNull();
  });

  it("leaves a same-page anchor to the body, but not a hash /chat/ link", () => {
    expect(linkFromClick(clickOn("#section-2"))).toBeNull();
    expect(linkFromClick(clickOn("#/chat/xyz-789"))?.href).toBe("#/chat/xyz-789");
  });

  it("keeps an off-site link, for the card to send to the browser", () => {
    expect(linkFromClick(clickOn("https://example.com/x"))?.resolved).toBe("https://example.com/x");
  });

  it("says when a modifier or non-primary button asked for a new tab", () => {
    for (const opts of [{ metaKey: true }, { ctrlKey: true }, { shiftKey: true }, { button: 1 }]) {
      expect(linkFromClick(clickOn("/chat/u4", opts))?.browserClick).toBe(true);
    }
  });
});

/// The frame's last listener: a click the app did not take is cancelled,
/// so no kind of link in a body follows itself, whatever the sanitizer
/// let through (`ChatBody.ce.vue`).
describe("finishFrameClick", () => {
  /** Whether a click on the element `pick` finds in `html` keeps the
   *  browser's own action after the frame's last listener. */
  function keptAfterClick(html: string, pick: string, init: MouseEventInit = {}): boolean {
    const root = document.createElement("div");
    root.innerHTML = html;
    document.body.append(root);
    document.addEventListener("click", finishFrameClick);
    document.addEventListener("auxclick", finishFrameClick);
    try {
      const ev = new MouseEvent(init.button === 1 ? "auxclick" : "click", {
        bubbles: true,
        cancelable: true,
        ...init,
      });
      root.querySelector(pick)!.dispatchEvent(ev);
      return !ev.defaultPrevented;
    } finally {
      document.removeEventListener("click", finishFrameClick);
      document.removeEventListener("auxclick", finishFrameClick);
      root.remove();
    }
  }

  it("cancels every kind of link, plain click or not", () => {
    const area = `<map name="m"><area id="t" shape="rect" coords="0,0,9,9" href="https://x.example/" target="_top"></map>`;
    const svg = `<svg><a xlink:href="https://x.example/" target="_top"><text id="t">s</text></a></svg>`;
    const math = `<math><mi id="t" href="https://x.example/">x</mi></math>`;
    const anchor = `<a href="https://x.example/" target="_top"><span id="t">a</span></a>`;
    for (const html of [area, svg, math, anchor]) {
      expect(keptAfterClick(html, "#t"), html).toBe(false);
      expect(keptAfterClick(html, "#t", { metaKey: true }), html).toBe(false);
      expect(keptAfterClick(html, "#t", { button: 1 }), html).toBe(false);
    }
  });

  it("keeps a summary opening its details, and a media element's controls", () => {
    expect(
      keptAfterClick(`<details><summary><b id="t">3 tool calls</b></summary></details>`, "#t"),
    ).toBe(true);
    expect(keptAfterClick(`<video id="t" controls></video>`, "#t")).toBe(true);
    expect(keptAfterClick(`<audio id="t" controls></audio>`, "#t")).toBe(true);
  });

  it("does not keep a link that sits in a summary", () => {
    expect(
      keptAfterClick(
        `<details><summary><a id="t" href="https://x.example/">x</a></summary></details>`,
        "#t",
      ),
    ).toBe(false);
  });

  it("scrolls a same-page anchor itself rather than let the frame navigate", () => {
    expect(keptAfterClick(`<a id="t" href="#fn-1">1</a><p id="fn-1">note</p>`, "#t")).toBe(false);
  });
});

describe("chatUuidFromHref", () => {
  it("accepts the three shapes renderers emit", () => {
    expect(chatUuidFromHref("/chat/abc")).toBe("abc");
    expect(chatUuidFromHref("#/chat/abc")).toBe("abc");
    expect(chatUuidFromHref("/#/chat/abc?x=1")).toBe("abc");
  });
  it("reads the document and the section out of a link to a section", () => {
    for (const href of ["/chat/abc#sec-1", "#/chat/abc#sec-1", "/chat/abc?x=1#sec-1"]) {
      expect(chatUuidFromHref(href)).toBe("abc");
      expect(chatSectionFromHref(href)).toBe("sec-1");
    }
  });
  it("finds no section in a link to a whole document or off the site", () => {
    expect(chatSectionFromHref("/chat/abc")).toBeNull();
    expect(chatSectionFromHref("/chat/abc?x=1")).toBeNull();
    expect(chatSectionFromHref("https://claude.ai/chat/abc#sec-1")).toBeNull();
  });
  it("ignores a card stack and an off-site link", () => {
    expect(chatUuidFromHref("/gridView()/documentView(%22abc%22)")).toBeNull();
    expect(chatUuidFromHref("https://claude.ai/chat/abc")).toBeNull();
  });
});

describe("rewriteIdButtonsForCopy", () => {
  /** A selection copied across a 🆔 keeps the 🆔 and its uuid, as a
   *  `datalib://` link to its section, or to the document for the page
   *  title's. */
  it("turns each copied 🆔 button into a link to what it names", () => {
    const root = document.createElement("div");
    root.innerHTML =
      `<h1 data-page-title-uuid="doc-1">Title <button type="button" class="copy-uuid" data-uuid="doc-1">🆔</button></h1>` +
      `<div data-section-uuid="sec-2"><button type="button" class="copy-uuid" data-uuid="sec-2">🆔</button> hello</div>`;
    expect(rewriteIdButtonsForCopy(root, "doc-1")).toBe(true);
    expect(root.querySelector("button")).toBeNull();
    const links = Array.from(root.querySelectorAll("a"), (a) => [
      a.getAttribute("href"),
      a.textContent,
    ]);
    expect(links).toEqual([
      ["datalib:doc/doc-1", "🆔 doc-1"],
      ["datalib:doc/doc-1#sec-2", "🆔 sec-2"],
    ]);
    expect(rewriteIdButtonsForCopy(root, "doc-1")).toBe(false);
  });

  /** The copy hook carries the rewrite to the clipboard's HTML, for a
   *  selection with no contact chip in it as much as one with. */
  it("puts the 🆔 link in a copied selection's html", () => {
    const root = document.createElement("div");
    root.innerHTML = `<p data-section-uuid="sec-2">hello <button type="button" class="copy-uuid" data-uuid="sec-2">🆔</button> there</p>`;
    document.body.append(root);
    const range = document.createRange();
    range.selectNodeContents(root);
    document.getSelection()!.removeAllRanges();
    document.getSelection()!.addRange(range);

    const data: Record<string, string> = {};
    let prevented = false;
    const ev = {
      clipboardData: { setData: (type: string, value: string) => (data[type] = value) },
      preventDefault: () => (prevented = true),
    } as unknown as ClipboardEvent;
    copyWithHandles(ev, root, (f) => rewriteIdButtonsForCopy(f, "doc-1"));
    root.remove();

    expect(prevented).toBe(true);
    expect(data["text/html"]).toContain('<a href="datalib:doc/doc-1#sec-2">🆔 sec-2</a>');
    expect(data["text/html"]).not.toContain("<button");
  });
});
