/**
 * Sending off-site links to the user's real browser.
 *
 * Rendered documents are full of links we did not author: the `↗`
 * outlink each renderer emits into a title / message header, and every
 * `<a>` that came out of the source content itself (a "Sent via
 * Superhuman" footer, a newsletter's tracking links). In a browser tab
 * those mostly do the right thing on their own. In the Tauri app both
 * shapes are broken, in opposite directions:
 *
 * - `target="_blank"` (what the `↗` carries) does **nothing**. The app
 *   has one webview and no tab strip, and Tauri does not implement
 *   `window.open`, so the click is silently swallowed — a link that
 *   looks like a link and isn't.
 * - a plain `<a href="https://…">` **navigates the app window** onto
 *   that site. The whole UI is replaced by someone's marketing page,
 *   with no back button in the chrome to return from it.
 *
 * So: intercept clicks on off-origin links and hand the URL to the OS.
 * In the app that's the opener plugin (default browser); in a browser
 * it's a new tab, which is what `target="_blank"` would have done and
 * what a bare in-body link arguably should have done too — a link in
 * someone else's email is not a reason to leave the app.
 *
 * `composedPath()` rather than `ev.target` is load-bearing: cards
 * render inside shadow roots (see `cards/vueCard.ts`), and an event
 * that crosses a shadow boundary is retargeted, so by the time it
 * reaches `document` its `target` is the card host element, not the
 * `<a>` the user clicked.
 */

import { openUrl } from "@tauri-apps/plugin-opener";
import { isDesktopApp } from "./desktop";

/**
 * Schemes that mean "leave the app": the web, and the two handoffs a
 * webview can't service itself. Everything else (`file:`, `data:`,
 * `blob:`, `javascript:`) is deliberately left to the host — see
 * `desktop.ts` for how local files are handled instead.
 */
const HANDOFF_SCHEMES = new Set(["mailto:", "tel:"]);

/**
 * Should a click on `href` leave the app?
 *
 * `base`/`origin` are parameters rather than reads of `document` so the
 * rule is testable without a DOM, and so callers that already hold a
 * document can pass its own base.
 */
export function isExternalHref(
  href: string,
  base: string,
  origin: string,
): boolean {
  if (!href) return false;
  let url: URL;
  try {
    url = new URL(href, base);
  } catch {
    return false;
  }
  if (url.protocol === "http:" || url.protocol === "https:") {
    // Same-origin http(s) is the app itself — routes, asset URLs,
    // `/#/chat/<uuid>` — and must keep navigating in place.
    return url.origin !== origin;
  }
  return HANDOFF_SCHEMES.has(url.protocol);
}

/**
 * Open `url` outside the app: the OS default browser in the desktop
 * app, a new tab in a browser.
 *
 * Never throws — a link that can't be opened should log, not blow up a
 * click handler.
 */
export async function openExternal(url: string): Promise<void> {
  if (isDesktopApp()) {
    try {
      await openUrl(url);
      return;
    } catch (e) {
      // Reachable when the opener capability is missing or its scope
      // stopped matching (see `datalib/tauri/capabilities/`). The
      // fallback below does nothing in the app, but the warning is how
      // we find out.
      console.warn("openUrl failed", e);
    }
  }
  window.open(url, "_blank", "noopener");
}

/** The nearest ancestor `<a href>` of the click, across shadow roots. */
function anchorFromClick(ev: MouseEvent): HTMLAnchorElement | null {
  for (const node of ev.composedPath()) {
    if (node instanceof HTMLAnchorElement && node.hasAttribute("href")) {
      return node;
    }
  }
  return null;
}

/**
 * Install the document-wide interceptor. Returns an uninstall function.
 *
 * Called once from `main.ts`; nothing else needs to know it exists.
 */
export function installExternalLinkHandler(doc: Document = document): () => void {
  const onClick = (ev: MouseEvent) => {
    if (ev.defaultPrevented || ev.button !== 0) return;
    // Modifier clicks are how a user asks the *browser* for a new tab
    // or a download, so leave them alone there. In the app there is no
    // such native behavior to preserve — cmd-click is another silent
    // no-op — so we take those too.
    const modified = ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey;
    if (modified && !isDesktopApp()) return;

    const a = anchorFromClick(ev);
    if (!a) return;
    // The literal attribute, not `a.href`: the resolved property would
    // turn every relative in-app link into an absolute same-origin URL
    // before we could tell them apart. (`isExternalHref` resolves it
    // itself, against the base we choose.)
    const href = a.getAttribute("href") ?? "";
    if (!isExternalHref(href, doc.baseURI, doc.location.origin)) return;

    ev.preventDefault();
    void openExternal(new URL(href, doc.baseURI).href);
  };

  doc.addEventListener("click", onClick);
  return () => doc.removeEventListener("click", onClick);
}
