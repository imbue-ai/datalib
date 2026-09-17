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
import DOMPurify from "dompurify";

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

export function sanitizeRenderedHtml(html: string): string {
  return DOMPurify.sanitize(html, {
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
}
