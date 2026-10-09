// What the Search card's controls decide, as pure functions over the
// query string, which is the card's only record of a search: the source
// chips read it and rewrite it, so a filter that was typed and one that
// was clicked are the same thing.
// Also which words of a snippet to mark.
import { filterToken, isFilterWord, queryWords, unquoteValue } from "@/grid/query";

/// The two predicates that carry free text (`datalib_unified_index`'s
/// `FreeTextMode`): words and meaning together, or meaning alone.
const HYBRID = "qmd:";
const MEANING = "qmd_vsearch:";
const SOURCE = "source_id:";

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

/// The one source `query` is narrowed to: the value of its `source_id:`
/// filter when it has exactly one. Null for none, or for several.
export function pickedSource(query: string): string | null {
  const picked = queryWords(query).filter((w) => w.startsWith(SOURCE));
  return picked.length === 1 ? carried(picked[0]) : null;
}

/// `query` narrowed to the source `id`, or to no source for null: every
/// `source_id:` filter it had is replaced. An excluded source
/// (`-source_id:`) is left alone.
export function setSource(query: string, id: string | null): string {
  const kept = queryWords(query).filter((w) => !w.startsWith(SOURCE));
  if (id !== null) kept.push(filterToken("source_id", id, false));
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
