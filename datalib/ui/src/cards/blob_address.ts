/**
 * The one kind of reference to our own origin a document body may load:
 * an attachment or a photo at its address in its source's blob store,
 * `/data/<group>/<function>/blob/<hash>`, or a part of an email stored
 * there, `…/blob/<hash>/part/<hash>` (`datalib/backend/blob_store/README.md`).
 * The server answers both; nothing in a body is rewritten to reach them.
 *
 * Its own module so the rule is unit-testable without mounting a
 * component.
 */

const NOWHERE = "http://own.invalid";
const BLOB_ADDRESS = /^\/data\/[^/]+\/[^/]+\/blob\/[0-9a-f]{64}(\/part\/[0-9a-f]{64})?$/;

/**
 * The blob-store address `value` names, or null. A body is whatever its
 * sender wrote, and anything it names on this origin is a GET with the
 * person's cookie, so the reference is resolved as the browser would
 * (`..`, `%2e%2e`, `\`) and must land exactly on an address. A relative
 * reference names nothing.
 */
export function blobAddress(value: string): string | null {
  const v = value.trim();
  if (!v.startsWith("/")) return null;
  let url: URL;
  try {
    url = new URL(v, `${NOWHERE}/`);
  } catch {
    return null;
  }
  if (url.origin !== NOWHERE || !BLOB_ADDRESS.test(url.pathname)) return null;
  return url.pathname;
}
