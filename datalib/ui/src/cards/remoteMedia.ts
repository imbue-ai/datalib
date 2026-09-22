// A rendered document's references to remote hosts — an `<img
// src="https://…">` in an email, a video in a chat, a CSS background —
// and what the page does with them. Loading one tells its host who
// opened the document and when (a tracking pixel is exactly that), so
// unless the server says an allow row covers it the sanitizer strips
// the reference, keeps it on the element as `data-remote-<attr>`, and
// the document view draws a placeholder naming the host. A covered
// reference is rewritten to `/api/remote_media?url=…` instead: the
// server checks the row again, fetches once into its download CAS and
// serves from there, so the app's CSP can forbid the page from
// reaching a remote host at all. Which rows cover what is decided on
// the server only (`http/src/remote_media.rs`); nothing here reads
// the rows. The rules are pure so they are unit-testable; only
// `decorateRemoteMedia` touches a DOM.
import { remoteMediaUrl, type RemoteContext } from "@/api";

export type { RemoteContext };

export type RemoteKind = "image" | "media" | "style";

export type RemoteRef = {
  url: string;
  host: string;
  kind: RemoteKind;
  /** Whether it was let through (proxied) rather than held. */
  loaded: boolean;
};

/** A URL on a host other than this page: `http:`, `https:`, or
 *  protocol-relative. Other schemes are the sanitizer's and the CSP's
 *  business, and a bare path is one of our own. */
export function isRemoteUrl(value: string): boolean {
  return /^(https?:|\/\/)/i.test(value.trim());
}

/** The URL as the server needs it: absolute, with a scheme. */
export function absoluteRemote(value: string): string {
  const v = value.trim();
  return v.startsWith("//") ? `https:${v}` : v;
}

export function hostOf(value: string): string {
  try {
    return new URL(absoluteRemote(value)).host;
  } catch {
    return "";
  }
}

export const NO_CONTEXT: RemoteContext = { document: null, source: null };

export function proxied(value: string, ctx: RemoteContext): string {
  return remoteMediaUrl(absoluteRemote(value), ctx);
}

// ── Rewriting ──────────────────────────────────────────────────────────

/** Every remote URL in a `srcset`, and the srcset with each mapped —
 *  `null` from `map` drops that candidate. */
export function rewriteSrcset(
  srcset: string,
  map: (url: string) => string | null,
): { srcset: string; remote: string[] } {
  const remote: string[] = [];
  const kept: string[] = [];
  for (const candidate of srcset.split(",")) {
    const parts = candidate.trim().split(/\s+/);
    const url = parts[0] ?? "";
    if (!url) continue;
    if (!isRemoteUrl(url)) {
      kept.push(candidate.trim());
      continue;
    }
    remote.push(url);
    const mapped = map(url);
    if (mapped !== null) kept.push([mapped, ...parts.slice(1)].join(" "));
  }
  return { srcset: kept.join(", "), remote };
}

const CSS_URL = /url\(\s*(['"]?)([^'")]*)\1\s*\)/gi;

/** Every remote `url(…)` in an inline style, and the style with each
 *  mapped — `null` from `map` replaces the whole `url(…)` with `none`,
 *  which is a valid value wherever an image was. */
export function rewriteStyleUrls(
  style: string,
  map: (url: string) => string | null,
): { style: string; remote: string[] } {
  const remote: string[] = [];
  const rewritten = style.replace(CSS_URL, (whole: string, _quote: string, url: string) => {
    if (!isRemoteUrl(url)) return whole;
    remote.push(url);
    const mapped = map(url);
    return mapped === null ? "none" : `url("${mapped}")`;
  });
  return { style: rewritten, remote };
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
  if (srcset) return rewriteSrcset(srcset, (u) => u).remote[0] ?? null;
  const style = el.getAttribute(`${BLOCKED_PREFIX}style`);
  if (style) return rewriteStyleUrls(style, (u) => u).remote[0] ?? null;
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
 *  is and where it would load from, the full URL on hover, and a
 *  click to let it load. Idempotent: an element that already has its
 *  placeholder gets no second one. */
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
    const chip = doc.createElement("button");
    chip.type = "button";
    chip.className = CHIP_CLASS;
    const pixel = isTrackingPixel(el);
    if (pixel) chip.classList.add(`${CHIP_CLASS}--pixel`);
    chip.dataset.remoteUrl = url;
    chip.title = `${absoluteRemote(url)}\nClick to load it from ${hostOf(url) || "its host"}.`;
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
