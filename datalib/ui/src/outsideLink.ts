// A name on its way out of the app, onto the clipboard, carries the
// library it is in, so the desktop app opens it there
// (datalib/backend/uri/README.md § "Which library"). Inside the app a
// name stays bare: it is always in the library that drew it.

import { healthSnapshot } from "@/api";
import { withLibrary } from "@/uri";

/** `name` with this library's id, or as it is before the server has
 *  said which library this is; a bare name opens in whichever library
 *  is open. */
export function outsideLink(name: string): string {
  const library = healthSnapshot()?.library_id;
  return library ? withLibrary(name, library) : name;
}
