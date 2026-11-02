// datalib's names: a `datalib:<kind>/<id>` link saying what something is
// (docs/dev/plans/datalib_uris.md). The Rust crate `datalib_uri` is the
// same; both read and write `tests/uri-fixtures.json` alike. The `doc`
// and `photo` kinds live here so far.

/** A rendered page, and a section of it: a message's uuid, its
 *  `data-section-uuid`. */
export type DocName = { page: string; section: string | null };

const DOC = "datalib:doc/";

export function writeDoc(name: DocName): string {
  return name.section === null ? DOC + name.page : `${DOC}${name.page}#${name.section}`;
}

/** `null` for anything that is not a `doc` name, never a guess. */
export function parseDoc(link: string): DocName | null {
  if (!link.startsWith(DOC)) return null;
  const rest = link.slice(DOC.length);
  const hash = rest.indexOf("#");
  const page = hash === -1 ? rest : rest.slice(0, hash);
  const section = hash === -1 ? "" : rest.slice(hash + 1);
  if (page === "" || page.includes("/")) return null;
  return { page, section: section === "" ? null : section };
}

const PHOTO = "datalib:photo/";

/** A photo or video, by its grid row's uuid. */
export function writePhoto(id: string): string {
  return PHOTO + id;
}

/** The photo's id, or `null` for anything that is not a `photo` name. */
export function parsePhoto(link: string): string | null {
  if (!link.startsWith(PHOTO)) return null;
  const id = link.slice(PHOTO.length);
  return id === "" || id.includes("/") || id.includes("#") ? null : id;
}
