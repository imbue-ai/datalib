// The frame a rendered document body is drawn in. The body is whatever
// a source sent — an email, a contact's name, a chat message — and the
// sanitizer is a parser that can be wrong. The frame is the boundary
// that does not depend on it: its document's policy is `script-src
// 'none'`, so a `<script>`, an `onerror=` or a `javascript:` link the
// sanitizer missed is inert, and it can load nothing from another host.
// The UI's own code still reaches in, so highlighting, copy buttons,
// edges and selection work as they did.
//
// Why a policy and not `sandbox` without `allow-scripts`: WebKit — the
// desktop app's engine — then runs no listener on the frame's document
// at all, the app's own included, so nothing in the body could be
// clicked. Chromium does run them. The policy blocks the document's
// script in both and leaves the app's listeners alone.
//
// A plot page framed in the body is a document of its own, under the
// policy the server sends it with: its own script, no network
// (`DocumentKind::Plot`, http/src/embed.rs). The sanitizer lets only a
// renderer's `plots/*.html` through, and this frame may frame nothing
// but the asset route.
import { UNIFIED_INDEX } from "@/api";
import themeCss from "@/theme.css?inline";
import hljsCss from "highlight.js/styles/github-dark.css?inline";
import bodyCss from "./documentBody.css?inline";
import chipCss from "./chip.css?inline";

/** The frame document's policy, on top of the app page's, which a
 *  `srcdoc` frame inherits. Never loosen `script-src`: the frame shares
 *  the app's origin, so a script that ran in it would hold the session. */
export function docFrameCsp(origin: string): string {
  return (
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; " +
    "img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; " +
    `frame-src ${origin}${UNIFIED_INDEX}/asset/; base-uri 'none'; form-action 'none'`
  );
}

// The frame is its own scroll pane, so a long message's sticky header
// sticks while the body scrolls. `.chat-preview` on the root is where
// `chatSections.js` looks for the pane.
const FRAME_CSS = `
html, body { background: transparent; min-height: 0; }
html.chat-preview { height: 100%; overflow-y: auto; }
body { margin: 0; padding: 0 1rem 0.75rem; }
`;

/** The document the frame starts as; the body is filled in afterwards,
 *  so a re-render keeps the scroll position and the decorations. */
export const DOC_FRAME_SRCDOC =
  `<!doctype html><html class="chat-preview"><head><meta charset="utf-8">` +
  `<meta http-equiv="Content-Security-Policy" content="${docFrameCsp(location.origin)}">` +
  `<style>${themeCss}\n${hljsCss}\n${bodyCss}\n${chipCss}\n${FRAME_CSS}</style></head>` +
  `<body class="chat-body markdown-body"></body></html>`;

/** An event's target as an Element, whichever window made it. An
 *  element inside the frame belongs to the frame's window, so
 *  `instanceof Element` (this window's) is false for it. */
export function asElement(target: EventTarget | Node | null | undefined): Element | null {
  const node = target as Node | null | undefined;
  if (!node || typeof node.nodeType !== "number") return null;
  if (node.nodeType === Node.ELEMENT_NODE) return node as Element;
  return node.parentElement;
}

/** Keep the frame's root on the app's density, now and as it changes.
 *  Colours need nothing: theme.css keys them off the colour scheme,
 *  which the frame shares. Returns the stop function. */
export function mirrorDensity(frameDoc: Document): () => void {
  const copy = () => {
    const d = document.documentElement.dataset.density;
    if (d) frameDoc.documentElement.dataset.density = d;
    else delete frameDoc.documentElement.dataset.density;
  };
  copy();
  const mo = new MutationObserver(copy);
  mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-density"] });
  return () => mo.disconnect();
}

/** A key the app answers wherever focus is (⌘K, Escape) reaches the
 *  app's window even when focus is inside the frame. Plain typing and
 *  the frame's own copy stay where they are. */
export function forwardAppKeys(frameDoc: Document): () => void {
  const onKey = (ev: KeyboardEvent) => {
    if (!(ev.metaKey || ev.ctrlKey || ev.key === "Escape")) return;
    window.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: ev.key,
        code: ev.code,
        metaKey: ev.metaKey,
        ctrlKey: ev.ctrlKey,
        shiftKey: ev.shiftKey,
        altKey: ev.altKey,
        bubbles: true,
        cancelable: true,
      }),
    );
  };
  frameDoc.addEventListener("keydown", onKey);
  return () => frameDoc.removeEventListener("keydown", onKey);
}
