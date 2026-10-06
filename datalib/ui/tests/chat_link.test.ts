import { describe, it, expect } from "vitest";
import { chatUuidFromHref, linkFromClick } from "../src/cards/chatLink";

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

describe("chatUuidFromHref", () => {
  it("accepts the three shapes renderers emit", () => {
    expect(chatUuidFromHref("/chat/abc")).toBe("abc");
    expect(chatUuidFromHref("#/chat/abc")).toBe("abc");
    expect(chatUuidFromHref("/#/chat/abc?x=1")).toBe("abc");
  });
  it("ignores a card stack and an off-site link", () => {
    expect(chatUuidFromHref("/gridView()/documentView(%22abc%22)")).toBeNull();
    expect(chatUuidFromHref("https://claude.ai/chat/abc")).toBeNull();
  });
});
