// datalib's names: a `datalib:<kind>/<id>` link saying what something is
// (docs/dev/plans/datalib_uris.md). The Rust crate `datalib_uri` is the
// same; both read and write `tests/uri-fixtures.json` alike. The `doc`
// and `photo` kinds live here so far.

/** A rendered page, and a section of it: a message's uuid, its
 *  `data-section-uuid`. */
export type DocName = { page: string; section: string | null };

const DOC = "datalib:doc/";

// What a page, a section and a photo are named by: a uuid
// (docs/dev/entity_ids.md). An id is put into a route, so one of any
// other shape (`..`) could name another route.
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** With a `title`, the page's uuid is written after it, as words and
 *  dashes (`datalib:doc/Ten-Forward-<uuid>`), for a person to read. The
 *  uuid alone says which page; a reader skips the words. */
export function writeDoc(name: DocName, title = ""): string {
  const slug = titleSlug(title);
  const page = slug ? `${slug}-${name.page}` : name.page;
  return name.section === null ? DOC + page : `${DOC}${page}#${name.section}`;
}

/** `null` for anything that is not a `doc` name, never a guess. */
export function parseDoc(link: string): DocName | null {
  if (!link.startsWith(DOC)) return null;
  const rest = link.slice(DOC.length);
  const hash = rest.indexOf("#");
  const titled = hash === -1 ? rest : rest.slice(0, hash);
  const section = hash === -1 ? "" : rest.slice(hash + 1);
  const page = titled.slice(-UUID_LEN);
  const words = titled.slice(0, -UUID_LEN);
  if (words !== "" && !SLUG.test(words)) return null;
  if (!UUID.test(page) || (section !== "" && !UUID.test(section))) return null;
  return { page, section: section === "" ? null : section };
}

const UUID_LEN = 36;

// What may stand before a page's uuid: anything up to a dash, but a
// character that would end the path or start another part of the link.
const SLUG = /^[^/?#]*-$/;

const SLUG_MAX = 60;

/** A title's ASCII letters and digits, a dash between each run, up to
 *  about sixty characters; accents dropped (`Café` is `Cafe`). Empty for
 *  a title with none, such as one in another script. */
export function titleSlug(title: string): string {
  const runs =
    title
      .normalize("NFKD")
      .replace(/\p{M}/gu, "")
      .match(/[A-Za-z0-9]+/g) ?? [];
  let out = "";
  for (const run of runs) {
    const next = out ? `${out}-${run}` : run;
    if (next.length > SLUG_MAX) return out || run.slice(0, SLUG_MAX);
    out = next;
  }
  return out;
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
  return UUID.test(id) ? id : null;
}
