// What the Search card's controls decide, as pure functions over the
// query string, which is the card's only record of a search: the source
// chips read it and rewrite it, so a filter that was typed and one that
// was clicked are the same thing.
// Also which words of a snippet to mark.
import { filterToken, isFilterWord, queryWords, unquoteValue } from "@/grid/query";

/// The older spellings of free text, which saved searches may hold.
const HYBRID = "qmd:";
const MEANING = "qmd_vsearch:";
/// `source:`, and `source_id:`, its older spelling a saved query may hold.
const SOURCE = /^(source|source_id):/;

const carriesText = (w: string) => w.startsWith(HYBRID) || w.startsWith(MEANING);
const carried = (w: string) => unquoteValue(w.slice(w.indexOf(":") + 1));

/// What `query` ranks: its bare words and what its `qmd:` and
/// `qmd_vsearch:` predicates carry, quotes off.
export function freeText(query: string): string {
  return queryWords(query)
    .filter((w) => carriesText(w) || !isFilterWord(w))
    .map((w) => (carriesText(w) ? carried(w) : unquoteValue(w)))
    .filter(Boolean)
    .join(" ");
}

/// The one source `query` is narrowed to: the value of its `source:`
/// filter when it has exactly one. Null for none, or for several.
export function pickedSource(query: string): string | null {
  const picked = queryWords(query).filter((w) => SOURCE.test(w));
  return picked.length === 1 ? carried(picked[0]) : null;
}

/// A search's tab name: the query, each value in it (a person, a date)
/// called what `nameOf` calls it where it knows, and a search for
/// everything with one person called that, as the menu entry that opens
/// it is.
export function searchTitle(query: string, nameOf: (value: string) => string | null): string {
  const named = queryWords(query).map((w) => {
    const colon = w.indexOf(":");
    if (colon <= 0) return { w, key: null, name: null };
    const key = w.slice(0, colon);
    const name = nameOf(unquoteValue(w.slice(colon + 1)));
    return { w: name ? `${key}:${name}` : w, key, name };
  });
  if (named.length === 0) return "Search";
  const [only] = named;
  if (named.length === 1 && only!.key === "with" && only!.name)
    return `Everything with ${only!.name}`;
  return `Search: ${named.map((n) => n.w).join(" ")}`;
}

/// `query` narrowed to the source `id`, or to no source for null: every
/// `source:` filter it had is replaced. An excluded source
/// (`-source:`) is left alone.
export function setSource(query: string, id: string | null): string {
  const kept = queryWords(query).filter((w) => !SOURCE.test(w));
  if (id !== null) kept.push(filterToken("source", id, false));
  return kept.join(" ");
}

/// The similarity a search by meaning needs with no `semantic:` of its own
/// (`Semantic::DEFAULT` in `datalib_unified_document_index`).
export const DEFAULT_CUTOFF = 0.3;
const SEMANTIC = /^semantic:/;

/// The cutoff `query` searches by meaning at, or null with `semantic:off`.
/// The last `semantic:` wins, as on the server.
export function semanticOf(query: string): number | null {
  const last = queryWords(query)
    .filter((w) => SEMANTIC.test(w))
    .pop();
  if (!last) return DEFAULT_CUTOFF;
  const value = carried(last);
  if (value === "off") return null;
  const cutoff = Number(value);
  return value === "on" || !Number.isFinite(cutoff) ? DEFAULT_CUTOFF : cutoff;
}

/// `query` at this cutoff, or with meaning left out for null. The default
/// is written as nothing, so a query is only as long as it says something.
export function setSemantic(query: string, cutoff: number | null): string {
  const kept = queryWords(query).filter((w) => !SEMANTIC.test(w));
  if (cutoff === null) kept.push("semantic:off");
  else if (cutoff !== DEFAULT_CUTOFF) kept.push(`semantic:${cutoff}`);
  return kept.join(" ");
}

const PER_DOCUMENT = "is:document";

export function isOneRowPerDocument(query: string): boolean {
  return queryWords(query).includes(PER_DOCUMENT);
}

export function setOneRowPerDocument(query: string, on: boolean): string {
  const kept = queryWords(query).filter((w) => w !== PER_DOCUMENT);
  if (on) kept.push(PER_DOCUMENT);
  return kept.join(" ");
}

export type Part = { text: string; hit: boolean };

/// `snippet` split so the words of `query` stand out. Matching is on
/// whole words, case-blind; a result found by meaning may mark nothing,
/// which is the truth about it.
export function markWords(snippet: string, query: string): Part[] {
  const words = [
    ...new Set(
      freeText(query)
        .toLowerCase()
        .split(/[^\p{L}\p{N}]+/u)
        .filter((w) => w.length > 1),
    ),
  ];
  if (words.length === 0) return [{ text: snippet, hit: false }];
  const escaped = words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const re = new RegExp(`(?<![\\p{L}\\p{N}])(${escaped.join("|")})(?![\\p{L}\\p{N}])`, "giu");
  const out: Part[] = [];
  let last = 0;
  for (const m of snippet.matchAll(re)) {
    const at = m.index ?? 0;
    if (at > last) out.push({ text: snippet.slice(last, at), hit: false });
    out.push({ text: m[0], hit: true });
    last = at + m[0].length;
  }
  if (last < snippet.length) out.push({ text: snippet.slice(last), hit: false });
  return out;
}
