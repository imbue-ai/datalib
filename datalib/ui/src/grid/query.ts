// The search-bar grammar every grid here shares, from this side of the
// wire: how a value is quoted into a `key:value` token, how a token is
// added to a query, and the two right-click entries that add one. The
// grammar itself is `datalib_query` on the backend; the unified grid's
// search sends it to `/applet/unified_index`, the run log's to `/api/log`.
/// `value` as one token: bare when it can be, double-quoted otherwise.
/// Mirrors `datalib_query::quote` (`\"` and `\\` escape inside quotes).
export function quoteValue(v: string): string {
  return /[\s:"]/.test(v) || v === "" || v.startsWith("-") ? quoted(v) : v;
}

/// A term's value may hold a colon, since a term splits at its first one
/// (`from:email:a@b.c`). Mirrors `datalib_query::term`, and with `whole`,
/// `datalib_query::exact_term`: always quoted, which a key that matches
/// a bare value in part reads as the whole value.
export function filterToken(key: string, value: string, exclude: boolean, whole = false): string {
  const bare = !whole && !(/[\s"]/.test(value) || value === "" || value.startsWith("-"));
  return `${exclude ? "-" : ""}${key}:${bare ? value : quoted(value)}`;
}

function quoted(v: string): string {
  return `"${v.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/// `query` with `token` appended as its own word — unless that exact
/// word is already there, in which case the query is returned as it is.
export function withToken(query: string, token: string): string {
  const current = query.trim();
  const re = new RegExp(`(^|\\s)${escapeRegExp(token)}(\\s|$)`);
  if (re.test(current)) return current;
  return current.length === 0 ? token : `${current} ${token}`;
}

/// `query` with every `key:…` word taken out, and `token` (or nothing)
/// put where the first of them was — for a key that means one thing at
/// a time, like a minimum level, which a control sets rather than adds.
export function replaceToken(query: string, key: string, token: string | null): string {
  const words = query
    .trim()
    .split(/\s+/)
    .filter((w) => w.length > 0);
  const prefix = `${key}:`;
  let placed = false;
  const kept: string[] = [];
  for (const w of words) {
    if (!w.startsWith(prefix)) {
      kept.push(w);
      continue;
    }
    if (token && !placed) kept.push(token);
    placed = true;
  }
  if (token && !placed) kept.push(token);
  return kept.join(" ");
}

/// The value of the first `key:value` word in `query`, unquoted only for
/// a bare value — a control reading its own token back.
export function tokenValue(query: string, key: string): string | null {
  const prefix = `${key}:`;
  const word = query
    .trim()
    .split(/\s+/)
    .find((w) => w.startsWith(prefix));
  return word == null ? null : word.slice(prefix.length);
}

/// A query's words: a quoted phrase is one, and so is a `key:"…"` filter.
const WORD = /-?[A-Za-z_][\w.]*:"(?:[^"\\]|\\.)*"?|"(?:[^"\\]|\\.)*"?|\S+/g;
const FILTER = /^-?[A-Za-z_][\w.]*:/;

/// `query` as its words, each as typed.
export function queryWords(query: string): string[] {
  return query.match(WORD) ?? [];
}

export function isFilterWord(word: string): boolean {
  return FILTER.test(word);
}

/// What a word says once its quotes are off: `"earl grey"` is earl grey.
export function unquoteValue(v: string): string {
  if (!v.startsWith('"')) return v;
  const inner = v.endsWith('"') && v.length > 1 ? v.slice(1, -1) : v.slice(1);
  return inner.replace(/\\(["\\])/g, "$1");
}

/// The words of `query` that are not `key:value` filters, in order: what
/// a search ranks as free text. The grammar itself is the backend's; this
/// only times a search, never decides one.
export function plainWords(query: string): string {
  return (query.match(WORD) ?? []).filter((w) => !FILTER.test(w)).join(" ");
}

/// How long to wait after a keystroke before searching. A change to the
/// free text is a qmd search when `ranked`: seconds long, one at a time,
/// and finished by the server even once the page has moved on. So it
/// waits for a real pause; a filter is a quick read and keeps up.
export function searchDelay(before: string, after: string, ranked: boolean): number {
  const words = plainWords(after);
  return ranked && words !== "" && words !== plainWords(before) ? 600 : 150;
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/// "Keep only" and "Exclude all" for one cell: each is a label and the
/// token it stands for. `shown` is the value as the cell displays it,
/// for the label; `value` is what the filter compares. The grid turns
/// them into menu entries; this knows nothing about menus.
export type FilterEntry = { label: string; token: string };

export function keepExcludeEntries(opts: {
  header: string;
  key: string;
  value: string;
  shown?: string;
  /// The key matches a bare value in part: the value is written whole.
  whole?: boolean;
}): FilterEntry[] {
  const shown = opts.shown ?? opts.value;
  const token = (exclude: boolean) => filterToken(opts.key, opts.value, exclude, opts.whole);
  return [
    { label: `Keep only ${opts.header}=${shown}`, token: token(false) },
    { label: `Exclude all ${opts.header}=${shown}`, token: token(true) },
  ];
}
