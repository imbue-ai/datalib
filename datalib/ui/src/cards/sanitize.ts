// The one place rendered markdown is made safe to put in the page.
//
// A message body reaches the markdown as the sender wrote it, and
// markdown-it runs with `html: true` because the section wrappers the
// renderers emit are HTML. So a message can carry any tag, and this page
// holds the API session. What is stripped: scripts, event handlers,
// `javascript:` URLs, foreign or inline-document iframes. What survives
// is the vocabulary the renderers actually emit — the `div.msg` section
// wrappers and their `data-*` ids, `details`/`summary`, `time`, `audio`
// and `video`, links with `target`, tables, and an `iframe` whose `src`
// is one of our own applet paths — plus whatever the UI decorates onto
// it afterwards.
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
// An iframe survives only when it frames a plot page a renderer wrote
// (`plots/*.html`). Anything else beside a markdown — an `.html`
// attachment above all — is what a sender sent, and framed it would sit
// inside the app's own chrome.
import DOMPurify from "dompurify";
import { UNIFIED_INDEX } from "../api";
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
  /** Which remote references may load, through the server — the
   *  server's own answer, asked beforehand. Absent: none. */
  accept?: (url: string) => boolean;
  /** What the body is, sent along with each load so the server can
   *  judge a `document` or `source` row. */
  context?: RemoteContext;
};

export type Sanitized = {
  html: string;
  /** Every remote reference the body carried, in document order,
   *  whether held or let through. */
  remote: RemoteRef[];
};

const ASSET_PREFIX = `${UNIFIED_INDEX}/asset/`;

/** `plots/<name>.html`, relative or under one markdown's asset route.
 *  A browser reads `%2e%2e` as `..` and `\` as `/`, so the name is
 *  judged decoded and a backslash refuses the whole value. */
export function isPlotPage(src: string): boolean {
  const v = src.trim();
  if (v.includes("\\") || /[?#]/.test(v)) return false;
  let segments = v.split("/");
  if (v.startsWith(ASSET_PREFIX)) segments = v.slice(ASSET_PREFIX.length).split("/").slice(1);
  if (segments.length !== 2 || segments[0] !== "plots") return false;
  const decoded = v.split("/").map((s) => {
    try {
      return decodeURIComponent(s);
    } catch {
      return "..";
    }
  });
  return (
    decoded.every((s) => s !== "." && s !== ".." && !s.includes("/")) &&
    segments[1].endsWith(".html")
  );
}

DOMPurify.addHook("uponSanitizeAttribute", (node, data) => {
  if (node.nodeName !== "IFRAME") return;
  if (data.attrName === "src" && !isPlotPage(data.attrValue)) {
    data.keepAttr = false;
  }
});

// DOMPurify's hooks are global and synchronous, so the options and the
// findings of the call in progress live here for its duration.
let accept: (url: string) => boolean = () => false;
let context: RemoteContext = NO_CONTEXT;
let found: RemoteRef[] = [];

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
  for (const attr of loadingAttributes(el.nodeName)) {
    const value = el.getAttribute(attr);
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

export function sanitizeRenderedHtml(html: string, options: SanitizeOptions = {}): Sanitized {
  accept = options.accept ?? (() => false);
  context = options.context ?? NO_CONTEXT;
  found = [];
  const clean = DOMPurify.sanitize(html, {
    // DOMPurify's own list plus the two schemes a chip link can carry:
    // Slack's deep link for a user, and `datalib:` for our own entities
    // (docs/dev/chips.md). An href in any other scheme is dropped.
    ALLOWED_URI_REGEXP:
      /^(?:(?:(?:f|ht)tps?|mailto|tel|callto|sms|cid|xmpp|matrix|slack|datalib):|[^a-z]|[a-z+.-]+(?:[^a-z+.\-:]|$))/i,
    // Not in DOMPurify's default set; the plot pages are iframes.
    ADD_TAGS: ["iframe"],
    // `target` is what makes an outlink open outside the app.
    ADD_ATTR: ["target", "controls", "datetime", "loading", "frameborder"],
    // An inline document would be a second page in our origin.
    FORBID_ATTR: ["srcdoc"],
    // No renderer emits a form or a control, and a form is a request the
    // page would send with its session attached. The copy button the UI
    // adds is injected after this runs, so it is unaffected.
    FORBID_TAGS: ["form", "input", "button", "select", "textarea", "style", "link", "meta", "base"],
  });
  const remote = found;
  found = [];
  return { html: clean, remote };
}
