// The search-bar grammar every grid here shares, from this side of the
// wire: how a value is quoted into a `key:value` token, how a token is
// added to a query, and the two right-click entries that add one. The
// grammar itself is `datalib_query` on the backend; the unified grid's
// search sends it to `/applet/unified_index`, the run log's to `/api/log`.
/// `value` as one token: bare when it can be, double-quoted otherwise.
/// Mirrors `datalib_query::quote` (`\"` and `\\` escape inside quotes).
export function quoteValue(v: string): string {
  const needsQuotes = v === "" || /[\s:"]/.test(v) || v.startsWith("-");
  if (!needsQuotes) return v;
  const escaped = v.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `"${escaped}"`;
}

export function filterToken(key: string, value: string, exclude: boolean): string {
  return `${exclude ? "-" : ""}${key}:${quoteValue(value)}`;
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
}): FilterEntry[] {
  const shown = opts.shown ?? opts.value;
  return [
    { label: `Keep only ${opts.header}=${shown}`, token: filterToken(opts.key, opts.value, false) },
    {
      label: `Exclude all ${opts.header}=${shown}`,
      token: filterToken(opts.key, opts.value, true),
    },
  ];
}
