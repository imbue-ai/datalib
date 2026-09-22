// A rendered document's references to remote hosts — an `<img
// src="https://…">` in an email, a video in a chat, a CSS background —
// and what the page does with them. Loading one tells its host who
// opened the document and when (a tracking pixel is exactly that), so
// the sanitizer strips the reference and keeps it on the element as
// `data-remote-<attr>`, and the document view draws a placeholder in
// its place naming the host. Nothing here loads anything: the app's
// CSP forbids the page from fetching from a remote host at all, and a
// load the person asks for will be a server-side fetch into a store
// (issue #648). The rules are pure so they are unit-testable; only
// `decorateRemoteMedia` touches a DOM.

export type RemoteKind = "image" | "media" | "style";

export type RemoteRef = {
  url: string;
  host: string;
  kind: RemoteKind;
};

/** A URL on a host other than this page: `http:`, `https:`, or
 *  protocol-relative. Other schemes are the sanitizer's and the CSP's
 *  business, and a bare path is one of our own. */
export function isRemoteUrl(value: string): boolean {
  return /^(https?:|\/\/)/i.test(value.trim());
}

export function hostOf(value: string): string {
  const v = value.trim();
  try {
    return new URL(v.startsWith("//") ? `https:${v}` : v).host;
  } catch {
    return "";
  }
}

/** The remote URLs among a `srcset`'s candidates. */
export function remoteInSrcset(srcset: string): string[] {
  return srcset
    .split(",")
    .map((candidate) => candidate.trim().split(/\s+/)[0] ?? "")
    .filter((url) => url !== "" && isRemoteUrl(url));
}

const CSS_URL = /url\(\s*(['"]?)([^'")]*)\1\s*\)/gi;

/** Every remote `url(…)` in an inline style, and the style with each
 *  replaced by `none` — a valid value wherever an image was. */
export function stripStyleUrls(style: string): { style: string; remote: string[] } {
  const remote: string[] = [];
  const stripped = style.replace(CSS_URL, (whole: string, _quote: string, url: string) => {
    if (!isRemoteUrl(url)) return whole;
    remote.push(url);
    return "none";
  });
  return { style: stripped, remote };
}

/** The attributes a browser fetches from, by element. `href` fetches
 *  only on the SVG image element (`<use>` cannot reach another origin);
 *  on `<a>` it is a link, which is the person's to click. */
export function loadingAttributes(nodeName: string): string[] {
  const name = nodeName.toLowerCase();
  if (name === "image") return ["href", "xlink:href"];
  return ["src", "poster", "background", "srcset", "style"];
}

export function kindOf(nodeName: string, attr: string): RemoteKind {
  const name = nodeName.toLowerCase();
  if (attr === "style") return "style";
  if (name === "video" || name === "audio" || name === "source" || name === "track") return "media";
  return "image";
}

const BLOCKED_PREFIX = "data-remote-";
export const BLOCKED_CLASS = "remote-blocked";
export const CHIP_CLASS = "remote-media";

/** Take a remote reference off the element, keeping it for the
 *  placeholder. `stripped` is what the attribute becomes, or nothing. */
export function blockAttribute(el: Element, attr: string, stripped: string | null): void {
  const original = el.getAttribute(attr);
  if (original === null) return;
  el.setAttribute(`${BLOCKED_PREFIX}${attr}`, original);
  if (stripped === null) el.removeAttribute(attr);
  else el.setAttribute(attr, stripped);
  el.classList.add(BLOCKED_CLASS);
}

/** The one URL a blocked element is shown by. */
export function primaryUrl(el: Element): string | null {
  for (const attr of ["src", "poster", "href", "xlink:href", "background"]) {
    const v = el.getAttribute(`${BLOCKED_PREFIX}${attr}`);
    if (v) return v;
  }
  const srcset = el.getAttribute(`${BLOCKED_PREFIX}srcset`);
  if (srcset) return remoteInSrcset(srcset)[0] ?? null;
  const style = el.getAttribute(`${BLOCKED_PREFIX}style`);
  if (style) return stripStyleUrls(style).remote[0] ?? null;
  return null;
}

/** A declared size of a pixel or two: an image there to be fetched,
 *  not seen. */
export function isTrackingPixel(el: Element): boolean {
  const dim = (attr: string) => {
    const v = el.getAttribute(attr);
    return v === null ? null : Number.parseInt(v, 10);
  };
  const w = dim("width");
  const h = dim("height");
  return w !== null && h !== null && w <= 2 && h <= 2;
}

/** A placeholder before every blocked image or media element: what it
 *  is and where it would load from, the full URL on hover. Idempotent:
 *  an element that already has its placeholder gets no second one. */
export function decorateRemoteMedia(root: ParentNode): void {
  for (const el of Array.from(
    root.querySelectorAll<Element>(
      `img.${BLOCKED_CLASS}, video.${BLOCKED_CLASS}, audio.${BLOCKED_CLASS}`,
    ),
  )) {
    if (el.previousElementSibling?.classList.contains(CHIP_CLASS)) continue;
    const url = primaryUrl(el);
    if (!url) continue;
    const doc = el.ownerDocument;
    const chip = doc.createElement("span");
    chip.className = CHIP_CLASS;
    const pixel = isTrackingPixel(el);
    if (pixel) chip.classList.add(`${CHIP_CLASS}--pixel`);
    chip.dataset.remoteUrl = url;
    chip.title = url;
    const icon = doc.createElement("span");
    icon.className = `${CHIP_CLASS}-icon`;
    icon.setAttribute("aria-hidden", "true");
    icon.textContent = el.nodeName.toLowerCase() === "img" ? "🖼" : "🎞";
    const host = doc.createElement("span");
    host.className = `${CHIP_CLASS}-host`;
    host.textContent = hostOf(url) || url;
    chip.append(icon, host);
    const alt = pixel ? "tracking pixel" : (el.getAttribute("alt") ?? "").trim();
    if (alt) {
      const label = doc.createElement("span");
      label.className = `${CHIP_CLASS}-alt`;
      label.textContent = alt;
      chip.append(label);
    }
    el.before(chip);
  }
}
