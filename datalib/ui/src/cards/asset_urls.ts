/**
 * Rewriting relative asset references inside a rendered markdown body to
 * the backend's asset route.
 *
 * Renderers write asset references **relative** to the markdown file
 * (`blobs/foo.png`, `plots/temperature.html`) so a rendered tree stays
 * usable on its own — opened off disk, served by a static file server,
 * or built by Quarto. The app serves those same files through
 * `/api/asset/{markdown_uuid}/{rel}`, so the body has to be rewritten on
 * the way into the DOM.
 *
 * Lives in its own module (rather than inline in `ChatBody.ce.vue`) so
 * the rules are unit-testable without mounting a component.
 */

/** Full URLs, protocol-relative URLs, absolute paths, and fragments. */
export function isAbsoluteOrUrl(src: string): boolean {
  return /^([a-z][a-z0-9+.-]*:|\/\/|\/|#)/i.test(src);
}

/** `blobs/foo.png` → `/api/asset/{uuid}/blobs/foo.png`. */
export function assetUrl(markdownUuid: string, src: string): string {
  return `/api/asset/${encodeURIComponent(markdownUuid)}/${src
    .split("/")
    .map(encodeURIComponent)
    .join("/")}`;
}

/**
 * `src` on an `<iframe>`, in a raw-HTML chunk.
 *
 * Images reach the renderer as markdown-it `image` tokens with parsed
 * attributes; raw HTML reaches it as an opaque string, so this has to
 * work on the markup. It is deliberately narrow — only `src`, only on
 * `iframe`, only when the value is relative — so nothing an author wrote
 * as an absolute URL is touched.
 *
 * The yolink renderer is what needs this: its page is a summary plus one
 * `<iframe src="plots/<quantity>.html">` per plot.
 */
const IFRAME_SRC = /(<iframe\b[^>]*?\bsrc\s*=\s*)(["'])([^"']*)\2/gi;

export function rewriteIframeSrcs(
  html: string,
  markdownUuid: string | null | undefined,
): string {
  if (!markdownUuid) return html;
  return html.replace(
    IFRAME_SRC,
    (whole: string, head: string, quote: string, src: string) =>
      src && !isAbsoluteOrUrl(src)
        ? `${head}${quote}${assetUrl(markdownUuid, src)}${quote}`
        : whole,
  );
}
