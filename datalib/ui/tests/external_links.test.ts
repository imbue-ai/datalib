import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  installExternalLinkHandler,
  isExternalHref,
  openExternal,
} from "../src/externalLinks";

const APP = "http://127.0.0.1:8765";
const BASE = `${APP}/`;

/** Install a fake Tauri IPC bridge; returns the invoke spy. */
function fakeTauri(impl?: (cmd: string, args?: unknown) => Promise<unknown>) {
  const invoke = vi.fn(impl ?? (() => Promise.resolve(null)));
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    invoke,
  };
  return invoke;
}

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.restoreAllMocks();
});

describe("isExternalHref", () => {
  it("treats an off-origin http(s) link as external", () => {
    // The two shapes from the bug report: a document's `↗` outlink, and
    // a link that came out of the email body itself.
    expect(
      isExternalHref("https://mail.superhuman.com/thread/abc", BASE, APP),
    ).toBe(true);
    expect(isExternalHref("https://superhuman.com", BASE, APP)).toBe(true);
    expect(isExternalHref("http://example.com/x", BASE, APP)).toBe(true);
  });

  it("keeps every in-app link internal", () => {
    // If any of these were treated as external, clicking a doc link
    // would pop the OS browser instead of navigating the card.
    for (const href of [
      "/#/chat/abc-123",
      "#/chat/abc-123",
      "/chat/abc-123",
      "/applet/unified_index/asset/md-1/blobs/x.png",
      `${APP}/#/grid`,
    ]) {
      expect(isExternalHref(href, BASE, APP), href).toBe(false);
    }
  });

  it("is origin-sensitive, not host-sensitive", () => {
    // A different port on the same host is a different server — likely
    // some other app entirely, not ours.
    expect(isExternalHref("http://127.0.0.1:9999/x", BASE, APP)).toBe(true);
  });

  it("hands off mailto: and tel:", () => {
    expect(isExternalHref("mailto:picard@enterprise.fed", BASE, APP)).toBe(true);
    expect(isExternalHref("tel:+15551234", BASE, APP)).toBe(true);
  });

  it("leaves other schemes to the host", () => {
    // `file:` has its own path (desktop.ts's reveal); the rest would be
    // actively harmful to hand to the OS.
    expect(isExternalHref("file:///corpus/a.pdf", BASE, APP)).toBe(false);
    expect(isExternalHref("data:text/plain,hi", BASE, APP)).toBe(false);
    expect(isExternalHref("javascript:void(0)", BASE, APP)).toBe(false);
    expect(isExternalHref("", BASE, APP)).toBe(false);
    expect(isExternalHref("not a url", BASE, APP)).toBe(false);
  });
});

describe("openExternal", () => {
  it("reaches the opener plugin's open_url command in the app", async () => {
    // Asserted through `@tauri-apps/plugin-opener` rather than around
    // it, for the same reason as desktop.test.ts: a Tauri major that
    // renames the command or its argument turns this test red instead
    // of turning the ↗ back into a silent no-op.
    const invoke = fakeTauri();
    await openExternal("https://superhuman.com/");
    const [cmd, args] = invoke.mock.calls[0];
    expect(cmd).toBe("plugin:opener|open_url");
    expect(args).toMatchObject({ url: "https://superhuman.com/" });
  });

  it("falls back to a new tab in a plain browser", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    await openExternal("https://superhuman.com/");
    expect(open).toHaveBeenCalledWith(
      "https://superhuman.com/",
      "_blank",
      "noopener",
    );
  });

  it("does not throw when the IPC call rejects", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    vi.spyOn(window, "open").mockReturnValue(null);
    fakeTauri(() => Promise.reject(new Error("not allowed")));
    await expect(openExternal("https://x.test/")).resolves.toBeUndefined();
  });
});

describe("installExternalLinkHandler", () => {
  let uninstall: () => void;
  let open: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    // jsdom serves the document from this origin (vite.config.ts sets
    // no other), so `document.location.origin` is what the handler
    // compares against.
    open = vi.spyOn(window, "open").mockReturnValue(null);
    uninstall = installExternalLinkHandler();
  });

  afterEach(() => {
    uninstall();
    document.body.innerHTML = "";
  });

  /** Click an `<a>` built from `html`, returning the event. */
  function clickAnchor(html: string, init: MouseEventInit = {}): MouseEvent {
    document.body.innerHTML = html;
    const a = document.body.querySelector("a")!;
    const ev = new MouseEvent("click", {
      bubbles: true,
      composed: true,
      cancelable: true,
      button: 0,
      ...init,
    });
    a.dispatchEvent(ev);
    return ev;
  }

  it("opens a target=_blank outlink instead of letting it no-op", () => {
    // The reported ↗: in the app, `target=_blank` needs `window.open`,
    // which Tauri does not implement, so the click did nothing.
    const ev = clickAnchor(
      '<a class="source-link" href="https://mail.superhuman.com/t/1" ' +
        'target="_blank" rel="noopener noreferrer">↗</a>',
    );
    expect(ev.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledWith(
      "https://mail.superhuman.com/t/1",
      "_blank",
      "noopener",
    );
  });

  it("opens a bare in-body link instead of navigating the app away", () => {
    // The reported "Superhuman" link: no `target`, so it replaced the
    // whole UI with the vendor's site.
    const ev = clickAnchor('<a href="https://superhuman.com">Superhuman</a>');
    expect(ev.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledOnce();
  });

  it("finds the anchor when the click lands on a child element", () => {
    document.body.innerHTML =
      '<a href="https://example.com/x"><span id="inner">go</span></a>';
    const ev = new MouseEvent("click", {
      bubbles: true,
      composed: true,
      cancelable: true,
    });
    document.getElementById("inner")!.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("crosses a shadow boundary, where ev.target is retargeted", () => {
    // Cards render inside shadow roots (cards/vueCard.ts). A handler
    // written against `ev.target` sees the host element here and the
    // whole fix silently stops applying to every rendered document.
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = host.attachShadow({ mode: "open" });
    root.innerHTML = '<a href="https://superhuman.com">Superhuman</a>';
    const a = root.querySelector("a")!;
    const ev = new MouseEvent("click", {
      bubbles: true,
      composed: true,
      cancelable: true,
    });
    a.dispatchEvent(ev);
    expect(ev.target).toBe(host);
    expect(ev.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledOnce();
  });

  it("leaves in-app links to the router", () => {
    const ev = clickAnchor('<a href="/#/chat/abc-123">a thread</a>');
    expect(ev.defaultPrevented).toBe(false);
    expect(open).not.toHaveBeenCalled();
  });

  it("leaves a click with no anchor alone", () => {
    document.body.innerHTML = "<div id='plain'>text</div>";
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    document.getElementById("plain")!.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
  });

  it("leaves modifier and middle clicks to the browser", () => {
    // cmd-click / middle-click are how a browser user asks for a new
    // tab; swallowing them would take that away.
    for (const init of [
      { metaKey: true },
      { ctrlKey: true },
      { shiftKey: true },
      { button: 1 },
    ]) {
      const ev = clickAnchor('<a href="https://example.com/x">x</a>', init);
      expect(ev.defaultPrevented, JSON.stringify(init)).toBe(false);
    }
    expect(open).not.toHaveBeenCalled();
  });

  it("takes modifier clicks in the app, where nothing native follows", () => {
    const invoke = fakeTauri();
    const ev = clickAnchor('<a href="https://example.com/x">x</a>', {
      metaKey: true,
    });
    expect(ev.defaultPrevented).toBe(true);
    expect(invoke.mock.calls[0][0]).toBe("plugin:opener|open_url");
  });

  it("stops intercepting once uninstalled", () => {
    uninstall();
    const ev = clickAnchor('<a href="https://example.com/x">x</a>');
    expect(ev.defaultPrevented).toBe(false);
    expect(open).not.toHaveBeenCalled();
  });
});
