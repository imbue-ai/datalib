// Links clicked in a rendered markdown body: which link, and whether it
// is one of the internal `/chat/<uuid>` links renderers emit. The
// document card decides where each goes. Also the link a copied 🆔
// button becomes.

import { chatDeeplink } from "@/router/deeplink";

// Accepts the three internal-link shapes our renderers emit:
//   /chat/<uuid>       — bare path (older claude / chatgpt / etc.)
//   #/chat/<uuid>      — bare hash (vue-router hash form)
//   /#/chat/<uuid>     — hash-prefixed absolute URL (perseus); a plain
//                        `<a href>` of this form would normally navigate
//                        to "/" + set the hash, which trips through to
//                        Vue Router but as a page-replace from the
//                        miller view's perspective.
const CHAT_HREF_RE = /^(?:#|\/#?)?\/chat\/([^/?#]+)/;

/**
 * A click the browser should keep: a modifier or a non-primary button
 * is how a person asks for a new tab or window, and the link's href
 * is what that tab shows.
 */
export function isBrowserClick(ev: MouseEvent): boolean {
  return ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0;
}

/** A link a click in a rendered body landed on: the href as written
 *  (`/chat/<uuid>` is matched on that), the URL the browser resolved,
 *  and whether the click asked for a new tab. Null when the click is
 *  not on a link, or on a same-page anchor, which the body scrolls to
 *  itself. Reads the target by node type: inside the document frame it
 *  belongs to the frame's window, where `instanceof Element` is false. */
export type ClickedLink = { href: string; resolved: string; browserClick: boolean };

export function linkFromClick(ev: MouseEvent): ClickedLink | null {
  const t = ev.target as Node | null;
  const el = t?.nodeType === Node.ELEMENT_NODE ? (t as Element) : (t?.parentElement ?? null);
  const a = el?.closest<HTMLAnchorElement>("a[href]");
  if (!a) return null;
  const href = a.getAttribute("href") ?? "";
  if (href.startsWith("#") && !chatUuidFromHref(href)) return null;
  return { href, resolved: a.href, browserClick: isBrowserClick(ev) };
}

/** The markdown uuid an internal `/chat/<uuid>` href names, or null. */
export function chatUuidFromHref(href: string): string | null {
  const m = CHAT_HREF_RE.exec(href);
  return m ? m[1] : null;
}

/** A copied 🆔 button (`chatSections.js`) becomes the same 🆔 as a
 *  `datalib://` link to what it names: the document, or the section
 *  within it. */
export function rewriteIdButtonsForCopy(
  fragment: DocumentFragment | Element,
  markdownUuid: string,
): boolean {
  const buttons = Array.from(fragment.querySelectorAll<HTMLElement>("button.copy-uuid[data-uuid]"));
  for (const btn of buttons) {
    const a = btn.ownerDocument.createElement("a");
    a.href = chatDeeplink(markdownUuid, btn.dataset.uuid ?? "");
    a.textContent = btn.textContent;
    btn.replaceWith(a);
  }
  return buttons.length > 0;
}
