// The one place rendered markdown is made safe to put in the page.
//
// A message body reaches the markdown as the sender wrote it, and
// markdown-it runs with `html: true` because the section wrappers the
// renderers emit are HTML. So a message can carry any tag, and this page
// holds the API session. What is stripped: scripts, event handlers,
// `javascript:` URLs, iframes. What survives
// is the vocabulary the renderers actually emit — the `div.msg` section
// wrappers and their `data-*` ids (a chart's `data-time-series` among
// them), `details`/`summary`, `time`, `audio` and `video`, links with
// `target="_blank"`, and tables — plus whatever the UI decorates onto it
// afterwards. No iframe survives: nothing a renderer writes frames a
// page, and an `.html` attachment framed would sit inside the app's own
// chrome.
//
// A link is an `<a>`, in one of the schemes the app opens, and opens in
// place or in a new window, never over the frame's parent: no `<area>`,
// no MathML `href`, no other `target`. What a click on one does is the
// document card's to decide (`chatLink.ts`).
//
// A chip — a link the app draws as a person, a group or a step — is
// marked only by the app's own markdown plugin (`chipLinks.js`), which
// signs its marks with the `chipMark` the caller passes; a mark the
// body's own HTML carries is taken off.
//
// A reference to a remote host — an image, a video, a CSS background —
// is not a script but a request the browser would make on the sender's
// behalf, telling them who opened the document and when. Unless the
// caller's `accept` says the person let it load, every such reference
// is taken off its element and kept as `data-remote-<attr>` for the
// document view to show in its place; one accepted is rewritten to the
// server's `/api/remote_media` route (`remoteMedia.ts`). Nothing leaves
// here pointing at a remote host, and the app's CSP refuses anything
// that still does.
//
// A reference to our own origin is a request too, sent with the
// person's cookie, so it loads only if it is an attachment's blob-store
// address (`blobAddress`). Anything else on this origin, a relative
// reference included, is dropped.
import DOMPurify from "dompurify";
import { blobAddress } from "./blob_address";
import {
  blockAttribute,
  hostOf,
  isRemoteUrl,
  kindOf,
  loadingAttributes,
  NO_CONTEXT,
  proxied,
  rewriteSrcset,
  rewriteStyleUrls,
  type RemoteContext,
  type RemoteRef,
} from "./remoteMedia";

export type SanitizeOptions = {
  /** The value the markdown plugin signed this render's chip links
   *  with. Absent: no link is a chip. */
  chipMark?: string;
  /** Which remote references may load, through the server — the
   *  server's own answer, asked beforehand. Absent: none. */
  accept?: (url: string) => boolean;
  /** What the body is, sent along with each load so the server can
   *  judge a `document` or `source` row. */
  context?: RemoteContext;
  /** The messages of the document this fragment is part of: an
   *  `id="m-…"` or `data-section-uuid` naming one is taken off, so only
   *  the renderer's own wrapper answers to it (`renderDocument.ts`). */
  reservedSections?: ReadonlySet<string>;
};

export type Sanitized = {
  html: string;
  /** Every remote reference the body carried, whether held or let
   *  through: in document order, but for a body rendered in sections,
   *  whose frame's come first. */
  remote: RemoteRef[];
};

// DOMPurify's hooks are global and synchronous, so the options and the
// findings of the call in progress live here for its duration.
let accept: (url: string) => boolean = () => false;
let context: RemoteContext = NO_CONTEXT;
let found: RemoteRef[] = [];
let chipMark: string | null = null;
let reserved: ReadonlySet<string> = new Set();

const CHIP_MARK = "data-chip-mark";
const CHIP_ATTRS = ["data-handle", "data-entity", "data-contact"];

// A presentation attribute of SVG's (`fill`, `filter`, `mask`, …) reads
// as CSS, so a `url(…)` in one is a fetch. One naming a part of the
// body itself (`url(#gradient)`) is not.
const SVG_LOCAL_REF = /^\s*url\(\s*(["']?)#[\w.:-]+\1\s*\)\s*$/i;
const SVG_REF = /url\(|image-set\(|\\/i;
const SVG_NS = "http://www.w3.org/2000/svg";

/** Neither remote nor another scheme (`data:`, `blob:`, which the CSP
 *  judges): a path the browser resolves against this origin. */
function isOwn(url: string): boolean {
  return !isRemoteUrl(url) && !/^[a-z][a-z0-9+.-]*:/i.test(url.trim());
}

function toOwn(url: string): string | null {
  return blobAddress(url);
}

function toProxy(url: string): string {
  return proxied(url, context);
}

function record(url: string, node: Node, attr: string, loaded: boolean): void {
  found.push({ url, host: hostOf(url), kind: kindOf(node.nodeName, attr), loaded });
}

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  const el = node as Element;
  if (!el.attributes) return;
  // A `data-remote-*` the source itself wrote would read as a reference
  // this page held.
  for (const name of Array.from(el.attributes, (a) => a.name)) {
    if (name.startsWith("data-remote-")) el.removeAttribute(name);
  }
  // `_blank` is the one target a link may name: any other loads the link
  // over the frame's parent, the whole app.
  if (el.hasAttribute("target") && el.getAttribute("target") !== "_blank") {
    el.removeAttribute("target");
  }
  keepOnlyRenderedChips(el);
  takeOffReservedIds(el);
  if (el.namespaceURI === SVG_NS) {
    for (const { name, value } of Array.from(el.attributes)) {
      if (name === "style" || loadingAttributes(el.nodeName).includes(name)) continue;
      if (SVG_REF.test(value) && !SVG_LOCAL_REF.test(value)) el.removeAttribute(name);
    }
  }
  for (const attr of loadingAttributes(el.nodeName)) {
    const raw = el.getAttribute(attr);
    if (raw === null) continue;
    const value = ownPass(el, attr, raw);
    if (value === null) continue;
    if (attr === "srcset") {
      // All or nothing: a srcset half held would still name a host.
      const { remote } = rewriteSrcset(value, (u) => u);
      if (remote.length === 0) continue;
      const all = remote.every(accept);
      for (const u of remote) record(u, node, attr, all);
      if (all) el.setAttribute(attr, rewriteSrcset(value, toProxy).srcset);
      else blockAttribute(el, attr, null);
    } else if (attr === "style") {
      const { style: stripped, remote } = rewriteStyleUrls(value, () => null);
      if (remote.length === 0) continue;
      const all = remote.every(accept);
      for (const u of remote) record(u, node, attr, all);
      if (all) el.setAttribute(attr, rewriteStyleUrls(value, toProxy).style);
      else blockAttribute(el, attr, stripped);
    } else if (isRemoteUrl(value)) {
      const loaded = accept(value);
      record(value, node, attr, loaded);
      if (loaded) el.setAttribute(attr, toProxy(value));
      else blockAttribute(el, attr, null);
    }
  }
});

function keepOnlyRenderedChips(el: Element): void {
  const signed = chipMark !== null && el.getAttribute(CHIP_MARK) === chipMark;
  el.removeAttribute(CHIP_MARK);
  if (signed) return;
  for (const name of CHIP_ATTRS) el.removeAttribute(name);
  if (el.classList.contains("chip")) {
    el.classList.remove("chip");
    if (el.classList.length === 0) el.removeAttribute("class");
  }
}

function takeOffReservedIds(el: Element): void {
  if (reserved.size === 0) return;
  const uuid = el.getAttribute("data-section-uuid");
  if (uuid !== null && reserved.has(uuid)) el.removeAttribute("data-section-uuid");
  const id = el.getAttribute("id");
  if (id?.startsWith("m-") && reserved.has(id.slice(2))) el.removeAttribute("id");
}

/** The attribute with its references to our own origin made safe, and
 *  written back; null when nothing is left of it. */
function ownPass(el: Element, attr: string, value: string): string | null {
  let next: string | null;
  if (attr === "srcset") next = rewriteSrcset(value, toOwn, isOwn).srcset || null;
  else if (attr === "style") next = rewriteStyleUrls(value, toOwn, isOwn).style;
  else if (isOwn(value)) next = toOwn(value);
  else next = value;
  if (next === value) return value;
  if (next === null) el.removeAttribute(attr);
  else el.setAttribute(attr, next);
  return next;
}

export function sanitizeRenderedHtml(html: string, options: SanitizeOptions = {}): Sanitized {
  accept = options.accept ?? (() => false);
  context = options.context ?? NO_CONTEXT;
  chipMark = options.chipMark ?? null;
  reserved = options.reservedSections ?? new Set();
  found = [];
  const clean = DOMPurify.sanitize(html, {
    // The schemes the app opens (`externalLinks.ts`), the two a chip link
    // can carry — Slack's deep link for a user, and `datalib:` for our
    // own entities (docs/dev/chips.md) — and a relative path. An href in
    // any other scheme is dropped. An email's `cid:` image the renderer
    // could not match to an attachment would load nothing anyway.
    ALLOWED_URI_REGEXP:
      /^(?:(?:https?|mailto|tel|slack|datalib):|[^a-z]|[a-z+.-]+(?:[^a-z+.\-:]|$))/i,
    // `target` is what makes an outlink open outside the app; the hook
    // keeps only `_blank`.
    ADD_ATTR: ["target", "controls", "datetime", "loading"],
    // An inline document would be a second page in our origin.
    FORBID_ATTR: ["srcdoc"],
    // No renderer emits a form or a control, and a form is a request the
    // page would send with its session attached. The copy button the UI
    // adds is injected after this runs, so it is unaffected.
    // An image map's `<area>` and MathML are links of other shapes, which
    // nothing a renderer writes uses.
    FORBID_TAGS: [
      "form",
      "input",
      "button",
      "select",
      "textarea",
      "style",
      "link",
      "meta",
      "base",
      "map",
      "area",
      "math",
    ],
  });
  const remote = found;
  found = [];
  chipMark = null;
  reserved = new Set();
  return { html: clean, remote };
}
