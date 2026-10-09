// A query's words with where each sits in the text, as the search field
// needs them: which word is a `key:value` term and where its value
// starts, and what the cursor is in the middle of typing. The grammar is
// `datalib_query`'s (`tokenize` and `parse`), mirrored over the same
// cases; this side only places words, it never decides a search.
import type { KeyValues, SearchKeySpec } from "@/api";
import { uriFromEntity } from "@/cards/chipLinks";
import { handleKind } from "@/cards/contacts";

export type Word = {
  /** The word as typed: `[from, to)` of the query. */
  from: number;
  to: number;
  negate: boolean;
  /** Null for free text. */
  key: string | null;
  /** A term's value, quotes off; free text as typed. */
  value: string;
  /** Where a term's value starts (just past its colon). */
  valueFrom: number;
};

/** The words of `query`, split as `datalib_query::tokenize` splits them:
 *  at whitespace outside double quotes, `\"` and `\\` escaping inside. */
export function words(query: string): Word[] {
  const out: Word[] = [];
  let start = -1;
  let inQuote = false;
  let escape = false;
  const end = (at: number) => {
    if (start >= 0) out.push(word(query, start, at));
    start = -1;
  };
  for (let i = 0; i < query.length; i++) {
    const ch = query[i];
    if (start < 0 && !/\s/.test(ch)) start = i;
    if (escape) {
      escape = false;
    } else if (ch === "\\" && inQuote) {
      escape = true;
    } else if (ch === '"') {
      inQuote = !inQuote;
    } else if (/\s/.test(ch) && !inQuote) {
      end(i);
    }
  }
  end(query.length);
  return out;
}

function word(query: string, from: number, to: number): Word {
  const text = query.slice(from, to);
  const negate = text.startsWith("-") && text.length > 1;
  const body = negate ? text.slice(1) : text;
  const colon = termColon(body);
  const free: Word = { from, to, negate: false, key: null, value: text, valueFrom: from };
  if (colon === null) return free;
  const key = body.slice(0, colon);
  const value = unquote(body.slice(colon + 1));
  if (key === "" || value === "") return free;
  return { from, to, negate, key, value, valueFrom: from + (negate ? 1 : 0) + colon + 1 };
}

/** Where a term splits: its first colon outside quotes. */
function termColon(body: string): number | null {
  let inQuote = false;
  let escape = false;
  for (let i = 0; i < body.length; i++) {
    const ch = body[i];
    if (escape) escape = false;
    else if (ch === "\\" && inQuote) escape = true;
    else if (ch === '"') inQuote = !inQuote;
    else if (ch === ":" && !inQuote) return i;
  }
  return null;
}

function unquote(s: string): string {
  if (s.length < 2 || !s.startsWith('"') || !s.endsWith('"')) return s;
  return s.slice(1, -1).replace(/\\(.)/g, "$1");
}

/** What the cursor is in the middle of typing: a key (`chan|`), or a
 *  key's value (`channel:br|`), with the span a pick replaces. For a
 *  value, `rest` is the query without this word, which narrows what is
 *  offered. Null in a gap between words, or in free text that cannot
 *  start a key. */
export type Completing =
  | { kind: "key"; from: number; to: number; typed: string }
  | { kind: "value"; key: string; from: number; to: number; typed: string; rest: string };

const KEY_START = /^[A-Za-z_][\w.]*$/;

export function completingAt(query: string, pos: number): Completing | null {
  const w = words(query).find((w) => w.from < pos && pos <= w.to);
  if (!w) return null;
  const bodyFrom = query[w.from] === "-" && w.to - w.from > 1 ? w.from + 1 : w.from;
  const body = query.slice(bodyFrom, w.to);
  const colon = termColon(body);
  if (colon === null || pos <= bodyFrom + colon) {
    const typed = query.slice(bodyFrom, pos);
    if (!KEY_START.test(typed)) return null;
    const to = colon === null ? w.to : bodyFrom + colon;
    return { kind: "key", from: bodyFrom, to, typed };
  }
  const from = bodyFrom + colon + 1;
  const typed = query.slice(from, pos).replace(/^"/, "");
  const rest = `${query.slice(0, w.from)}${query.slice(w.to)}`.replace(/\s+/g, " ").trim();
  return { kind: "value", key: body.slice(0, colon), from, to: w.to, typed, rest };
}

/** A term's value as the query spells it: bare when it reads back as
 *  itself, quoted otherwise. Mirrors `datalib_query::term`. */
export function termValue(value: string): string {
  const bare = !(/[\s"]/.test(value) || value === "" || value.startsWith("-"));
  return bare ? value : `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/** The key `typed` names, by its name or an alias. */
export function keyNamed(keys: SearchKeySpec[], typed: string): SearchKeySpec | undefined {
  return keys.find((k) => k.key === typed || k.aliases.includes(typed));
}

/** What a chip names: a source, group or step by its entity URI, or a
 *  person by their handle. */
export type ChipRef = { kind: "entity"; uri: string } | { kind: "person"; handle: string };

/** The resolver key a chip is asked about by. */
export function chipKey(chip: ChipRef): string {
  return chip.kind === "entity" ? chip.uri : chip.handle;
}

/** The chip a value of a key with these values names; null for a value
 *  drawn as text. A person's value is a chip only when it is a handle as
 *  `datalib_handle` spells it: anything else matches in part. */
export function chipFor(values: KeyValues, value: string): ChipRef | null {
  switch (values.kind) {
    case "source":
    case "group":
      return { kind: "entity", uri: uriFromEntity("group", value) };
    case "step":
      return { kind: "entity", uri: uriFromEntity("step", value) };
    case "person":
      return handleKind(value) ? { kind: "person", handle: value } : null;
    default:
      return null;
  }
}

/** The words of `query` drawn as chips, each with what it names. */
export function chipWords(query: string, keys: SearchKeySpec[]): { word: Word; chip: ChipRef }[] {
  const out: { word: Word; chip: ChipRef }[] = [];
  for (const w of words(query)) {
    if (w.key === null) continue;
    const spec = keyNamed(keys, w.key);
    const chip = spec ? chipFor(spec.values, w.value) : null;
    if (chip) out.push({ word: w, chip });
  }
  return out;
}
