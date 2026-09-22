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
// caller says the source is trusted, every such reference is taken off
// its element and kept as `data-remote-<attr>` for the document view
// to offer (`remoteMedia.ts`); when the caller says to load, it goes
// through `/api/remote` instead. Either way nothing here points at a
// remote host, and the app's CSP refuses anything that still does.
import DOMPurify from "dompurify";
import {
  blockAttribute,
  hostOf,
  isRemoteUrl,
  kindOf,
  loadedValue,
  loadingAttributes,
  rewriteSrcset,
  rewriteStyleUrls,
  type RemoteRef,
} from "./remoteMedia";

export type SanitizeOptions = {
  /** Put remote references back, proxied, rather than holding them. */
  loadRemote: boolean;
};

export type Sanitized = {
  html: string;
  /** Every remote reference the body carried, in document order,
   *  whether held or loaded. */
  remote: RemoteRef[];
};

const SCHEME = /^[a-z][a-z0-9+.-]*:/i;

/** True for a URL the page itself serves: a relative path or a
 *  root-relative one, never a scheme and never protocol-relative. */
function isOwnPath(value: string): boolean {
  const v = value.trim();
  return v !== "" && !SCHEME.test(v) && !v.startsWith("//");
}

DOMPurify.addHook("uponSanitizeAttribute", (node, data) => {
  if (node.nodeName !== "IFRAME") return;
  if (data.attrName === "src" && !isOwnPath(data.attrValue)) {
    data.keepAttr = false;
  }
});

// DOMPurify's hooks are global and synchronous, so the options and the
// findings of the call in progress live here for its duration.
let current: SanitizeOptions = { loadRemote: false };
let found: RemoteRef[] = [];

function note(url: string, node: Node, attr: string): void {
  found.push({
    url,
    host: hostOf(url),
    kind: kindOf(node.nodeName, attr),
    loaded: current.loadRemote,
  });
}

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  const el = node as Element;
  if (!el.attributes) return;
  // A `data-remote-*` the source itself wrote would read as a reference
  // this page held, and be offered for loading.
  for (const name of Array.from(el.attributes, (a) => a.name)) {
    if (name.startsWith("data-remote-")) el.removeAttribute(name);
  }
  for (const attr of loadingAttributes(el.nodeName)) {
    const value = el.getAttribute(attr);
    if (value === null) continue;
    if (attr === "srcset") {
      const { remote } = rewriteSrcset(value, (u) => u);
      if (remote.length === 0) continue;
      for (const u of remote) note(u, node, attr);
      if (current.loadRemote) el.setAttribute(attr, loadedValue(attr, value));
      else blockAttribute(el, attr, null);
    } else if (attr === "style") {
      const { style, remote } = rewriteStyleUrls(value, () => null);
      if (remote.length === 0) continue;
      for (const u of remote) note(u, node, attr);
      if (current.loadRemote) el.setAttribute(attr, loadedValue(attr, value));
      else blockAttribute(el, attr, style);
    } else {
      if (!isRemoteUrl(value)) continue;
      note(value, node, attr);
      if (current.loadRemote) el.setAttribute(attr, loadedValue(attr, value));
      else blockAttribute(el, attr, null);
    }
  }
});

export function sanitizeRenderedHtml(
  html: string,
  options: SanitizeOptions = { loadRemote: false },
): Sanitized {
  current = options;
  found = [];
  const clean = DOMPurify.sanitize(html, {
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
