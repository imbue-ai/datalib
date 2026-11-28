// `/name/<kind>/<id>`: the fixed address of a `datalib:` name
// (datalib/backend/uri/README.md § "The fixed address"). A browser, a
// bookmark or the desktop app's link handler lands here, and the router
// opens what the name names. This file decides what that is; the router
// fetches what it needs and opens it.

import { parseDoc, parseNode, parsePhoto, splitLibrary } from "@/uri";

export const NAME_PATH = "/name/";

/** The `datalib:` link a `/name/…` path spells, its query and fragment
 *  included; null for any other path. */
export function linkOfPath(fullPath: string): string | null {
  return fullPath.startsWith(NAME_PATH) ? `datalib:${fullPath.slice(NAME_PATH.length)}` : null;
}

/** The fixed address of a `datalib:` link, as the server serves it. */
export function pathOfLink(link: string): string | null {
  return link.startsWith("datalib:") ? NAME_PATH + link.slice("datalib:".length) : null;
}

export type Named =
  | { kind: "doc"; page: string; section: string | null }
  | { kind: "node"; id: string }
  | { kind: "photo"; id: string };

/** What a link opens here, or why it opens nothing. */
export type NameRoute = { open: Named } | { refused: string };

/** What `link` opens in the library whose id is `here`. A link that names
 *  no library opens in this one. */
export function routeOfLink(link: string, here: string | null): NameRoute {
  const split = splitLibrary(link);
  if (!split) return { refused: `Datalib cannot read the link ${link}` };
  if (split.library !== null && split.library.toLowerCase() !== here?.toLowerCase()) {
    return {
      refused:
        "That link is for another library. Open it from the desktop app, " +
        "or open that library's own Datalib.",
    };
  }
  const doc = parseDoc(split.name);
  if (doc) return { open: { kind: "doc", ...doc } };
  const node = parseNode(split.name);
  if (node) return { open: { kind: "node", id: node } };
  const photo = parsePhoto(split.name);
  if (photo) return { open: { kind: "photo", id: photo } };
  return { refused: `This build of Datalib does not know what ${split.name} names` };
}
