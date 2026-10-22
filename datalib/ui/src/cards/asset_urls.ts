/**
 * Rewriting relative asset references inside a rendered markdown body to
 * the backend's asset route.
 *
 * An attachment its source's blob store holds is linked at its address,
 * `/data/<group>/<function>/blob/<hash>`, which the server answers and
 * which needs no rewriting. What is left relative is a file a renderer
 * wrote beside the page (`blobs/foo.png`), served through the
 * `unified_document_index` applet's `/asset/{markdown_uuid}/{rel}`, so
 * the body has to be rewritten on the way into the DOM.
 *
 * Lives in its own module (rather than inline in `ChatBody.ce.vue`) so
 * the rules are unit-testable without mounting a component.
 */

import { UNIFIED_DOCUMENT_INDEX } from "../api";

/** Full URLs, protocol-relative URLs, absolute paths, and fragments. */
export function isAbsoluteOrUrl(src: string): boolean {
  return /^([a-z][a-z0-9+.-]*:|\/\/|\/|#)/i.test(src);
}

/** `blobs/foo.png` → `/applet/unified_document_index/asset/{uuid}/blobs/foo.png`. */
export function assetUrl(markdownUuid: string, src: string): string {
  return `${UNIFIED_DOCUMENT_INDEX}/asset/${encodeURIComponent(markdownUuid)}/${src
    .split("/")
    .map(encodeURIComponent)
    .join("/")}`;
}

const ASSETS = `${UNIFIED_DOCUMENT_INDEX}/asset/`;
const NOWHERE = "http://own.invalid";
/** An attachment in its source's blob store, which the server answers
 *  (`datalib/backend/blob_store/README.md`). */
const BLOB_ADDRESS = /^\/data\/[^/]+\/[^/]+\/blob\/[0-9a-f]{64}$/;

/**
 * What a body's reference to our own origin may load, or null. A body is
 * whatever its sender wrote, and anything it names on this origin is a
 * GET with the person's cookie, so only a document's assets may load:
 * the reference is resolved as the browser would (`..`, `%2e%2e`, `\`)
 * and must land on an attachment's blob-store address or under the asset
 * route. A relative one is the document's own (`blobs/a.mp3`) and must
 * stay in its directory.
 */
export function ownAssetReference(value: string, markdownUuid: string | null): string | null {
  const v = value.trim();
  const relative = !v.startsWith("/");
  if (relative && !markdownUuid) return null;
  const candidate = relative ? assetUrl(markdownUuid!, v) : v;
  const within = relative ? `${ASSETS}${encodeURIComponent(markdownUuid!)}/` : ASSETS;
  let url: URL;
  try {
    url = new URL(candidate, `${NOWHERE}/`);
  } catch {
    return null;
  }
  if (url.origin !== NOWHERE) return null;
  if (!relative && BLOB_ADDRESS.test(url.pathname)) return url.pathname;
  if (!url.pathname.startsWith(within)) return null;
  return url.pathname + url.search;
}
