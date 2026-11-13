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
 * - `target="_blank"` (what the `↗` carries) reaches the shell's
 *   `on_new_window` handler (`datalib/tauri/src/main.rs`), which opens
 *   a page of the app as a second window of the app, hands a web, mail
 *   or phone link to the OS, and refuses anything else. Before that handler existed the click was
 *   silently swallowed — a webview has no tab strip.
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
 * The schemes a link may leave the app by: the web, mail and phone. A
 * body's link can name any other, a chip's `slack://` and `datalib:`
 * among them, and the OS would hand each to whatever app claims it. The
 * desktop app's opener is scoped to the same four
 * (`capabilities/open-external-urls.json`), and so is its shell
 * (`datalib/tauri/src/navigation.rs`). `file:` has its own way out, the
 * grid's reveal (`desktop.ts`).
 */
const LEAVING_SCHEMES = new Set(["http:", "https:", "mailto:", "tel:"]);

/**
 * Should a click on `href` leave the app?
 *
 * `base`/`origin` are parameters rather than reads of `document` so the
 * rule is testable without a DOM, and so callers that already hold a
 * document can pass its own base.
 */
export function isExternalHref(href: string, base: string, origin: string): boolean {
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
  return LEAVING_SCHEMES.has(url.protocol);
}

/** A page on the web: what the grid's "Open source" opens. A row's
 *  `source_url` is what its source wrote, in whatever scheme. */
export function isWebUrl(url: string): boolean {
  try {
    const { protocol } = new URL(url);
    return protocol === "http:" || protocol === "https:";
  } catch {
    return false;
  }
}

/**
 * Open `url` outside the app: the OS default browser in the desktop
 * app, a new tab in a browser. Any scheme but the four above is refused.
 *
 * Never throws — a link that can't be opened should log, not blow up a
 * click handler.
 */
export async function openExternal(url: string): Promise<void> {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    console.warn("not opening a link that does not parse", url);
    return;
  }
  if (!LEAVING_SCHEMES.has(parsed.protocol)) {
    console.warn(`not opening a ${parsed.protocol} link outside the app`);
    return;
  }
  if (!isDesktopApp()) {
    window.open(parsed.href, "_blank", "noopener,noreferrer");
    return;
  }
  try {
    await openUrl(parsed.href);
  } catch (e) {
    // Reachable when the opener capability is missing or its scope
    // stopped matching (see `datalib/tauri/capabilities/`). There is no
    // fallback: `window.open` in the app is a window of the app.
    console.warn("openUrl failed", e);
  }
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
