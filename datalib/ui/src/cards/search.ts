// What the Search card decides, as pure functions: the query it sends
// for what was typed and picked, what it keeps in its state string, and
// which words of a snippet to mark.
import { filterToken } from "@/grid/query";
import { DEFAULT_QUERY } from "./searchDefaults";

/// A `key:value` word (possibly negated) rather than free text — the
/// same shape `datalib_query` treats as a filter.
const FILTER_WORD = /^-?[A-Za-z_]+:/;

export type SearchInput = {
  // What the person typed: free text, filters, or both.
  text: string;
  // Rank the free text by meaning alone, not words and meaning together.
  meaningOnly: boolean;
  // The source picked from the chips; null for every source.
  sourceId: string | null;
};

/// The free text of `text`, with its filter words taken out.
export function freeText(text: string): string {
  return text
    .trim()
    .split(/\s+/)
    .filter((w) => w && !FILTER_WORD.test(w))
    .join(" ");
}

/// The query the search endpoint gets. Nothing typed browses every
/// document; "meaning only" moves the free text into a `qmd_vsearch:`
/// predicate, leaving the filters as they are.
export function searchQuery(input: SearchInput, withSource = true): string {
  const words = input.text.trim().split(/\s+/).filter(Boolean);
  const filters = words.filter((w) => FILTER_WORD.test(w));
  const free = words.filter((w) => !FILTER_WORD.test(w)).join(" ");
  const parts = [...filters];
  if (free) parts.push(input.meaningOnly ? filterToken("qmd_vsearch", free, false) : free);
  else if (filters.length === 0) parts.push(DEFAULT_QUERY);
  if (withSource && input.sourceId) parts.push(filterToken("source_id", input.sourceId, false));
  return parts.join(" ");
}

/// The card's state string: what it needs to come back as it was.
export function encodeSearchState(input: SearchInput): string {
  const p = new URLSearchParams();
  if (input.text) p.set("q", input.text);
  if (input.meaningOnly) p.set("m", "1");
  if (input.sourceId) p.set("src", input.sourceId);
  return p.toString();
}

export function decodeSearchState(state: string, fallbackText: string): SearchInput {
  const p = new URLSearchParams(state);
  return {
    text: p.get("q") ?? fallbackText,
    meaningOnly: p.get("m") === "1",
    sourceId: p.get("src"),
  };
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
