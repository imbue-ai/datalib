// datalib's names: a `datalib:<kind>/<id>` link saying what something is
// (docs/dev/plans/datalib_uris.md). The Rust crate `datalib_uri` is the
// same; both read and write `tests/uri-fixtures.json` alike. Only the
// `doc` kind lives here so far.

/** A rendered page, a section of it (a message's uuid, its
 *  `data-section-uuid`), and words to find inside that section. */
export type DocName = { page: string; section: string | null; text: string | null };

const DOC = "datalib:doc/";
/** The web's text-fragment directive, which a section may carry. */
const TEXT = ":~:text=";

export function writeDoc(name: DocName): string {
  let out = DOC + name.page;
  if (name.section !== null || name.text !== null) out += "#" + (name.section ?? "");
  if (name.text !== null) out += TEXT + encodeURIComponent(name.text);
  return out;
}

/** `null` for anything that is not a `doc` name, never a guess. */
export function parseDoc(link: string): DocName | null {
  if (!link.startsWith(DOC)) return null;
  const rest = link.slice(DOC.length);
  const hash = rest.indexOf("#");
  const page = hash === -1 ? rest : rest.slice(0, hash);
  const fragment = hash === -1 ? "" : rest.slice(hash + 1);
  if (page === "" || page.includes("/")) return null;
  const at = fragment.indexOf(TEXT);
  const section = at === -1 ? fragment : fragment.slice(0, at);
  let text: string | null = null;
  if (at !== -1) {
    try {
      text = decodeURIComponent(fragment.slice(at + TEXT.length));
    } catch {
      return null;
    }
  }
  return { page, section: section === "" ? null : section, text: text === "" ? null : text };
}
