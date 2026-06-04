import { describe, it, expect } from "vitest";
import { chatHrefFromClick } from "../src/router/chat_link";

// Forge a MouseEvent-shaped object whose `target` is an `<a>` carrying
// the given href (or an arbitrary descendant of it). jsdom's
// `MouseEvent` doesn't let you assign `target` after construction, so
// we cast a plain object literal instead — `chatHrefFromClick` only
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

describe("chatHrefFromClick", () => {
  it("returns the uuid for a /chat/<uuid> link", () => {
    expect(chatHrefFromClick(clickOn("/chat/abc-123"))).toBe("abc-123");
  });

  it("returns the uuid for a hash-prefixed /chat/<uuid> link", () => {
    expect(chatHrefFromClick(clickOn("#/chat/xyz-789"))).toBe("xyz-789");
  });

  it("strips trailing query / hash from the uuid segment", () => {
    expect(chatHrefFromClick(clickOn("/chat/u1?msg=42"))).toBe("u1");
    expect(chatHrefFromClick(clickOn("/chat/u1#m5"))).toBe("u1");
    expect(chatHrefFromClick(clickOn("/chat/u1/extra"))).toBe("u1");
  });

  it("walks up to find an ancestor <a>", () => {
    expect(chatHrefFromClick(clickOn("/chat/u2", {}, true))).toBe("u2");
  });

  it("returns null when the click isn't inside an <a>", () => {
    const div = document.createElement("div");
    const ev = { target: div, button: 0 } as unknown as MouseEvent;
    expect(chatHrefFromClick(ev)).toBeNull();
  });

  it("returns null for non-/chat hrefs", () => {
    expect(chatHrefFromClick(clickOn("/other/u3"))).toBeNull();
    expect(chatHrefFromClick(clickOn("https://example.com/chat/u3"))).toBeNull();
    expect(chatHrefFromClick(clickOn(""))).toBeNull();
  });

  it("returns null for a missing href attribute", () => {
    expect(chatHrefFromClick(clickOn(null))).toBeNull();
  });

  it("returns null when a modifier or non-primary button is held", () => {
    expect(chatHrefFromClick(clickOn("/chat/u4", { metaKey: true }))).toBeNull();
    expect(chatHrefFromClick(clickOn("/chat/u4", { ctrlKey: true }))).toBeNull();
    expect(chatHrefFromClick(clickOn("/chat/u4", { shiftKey: true }))).toBeNull();
    expect(chatHrefFromClick(clickOn("/chat/u4", { button: 1 }))).toBeNull();
  });

  it("returns null for a non-Element event target", () => {
    expect(chatHrefFromClick({ target: null } as unknown as MouseEvent)).toBeNull();
  });
});
