/**
 * Rewriting relative asset references inside a rendered markdown body to
 * the backend's asset route.
 *
 * Renderers write asset references **relative** to the markdown file
 * (`blobs/foo.png`) so a rendered tree stays
 * usable on its own — opened off disk, served by a static file server,
 * or built by Quarto. The app serves those same files through
 * the `unified_document_index` applet's `/asset/{markdown_uuid}/{rel}`, so the
 * body has to be rewritten on
 * the way into the DOM.
 *
 * Lives in its own module (rather than inline in `ChatBody.ce.vue`) so
 * the rules are unit-testable without mounting a component.
 */

import { UNIFIED_DOCUMENT_INDEX } from "../api";
import { withToken } from "../apiToken";

/** Full URLs, protocol-relative URLs, absolute paths, and fragments. */
export function isAbsoluteOrUrl(src: string): boolean {
  return /^([a-z][a-z0-9+.-]*:|\/\/|\/|#)/i.test(src);
}

/** `blobs/foo.png` → `/applet/unified_document_index/asset/{uuid}/blobs/foo.png`. */
export function assetUrl(markdownUuid: string, src: string): string {
  return withToken(
    `${UNIFIED_DOCUMENT_INDEX}/asset/${encodeURIComponent(markdownUuid)}/${src
      .split("/")
      .map(encodeURIComponent)
      .join("/")}`,
  );
}
