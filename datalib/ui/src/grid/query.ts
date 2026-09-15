// The search-bar grammar every grid here shares, from this side of the
// wire: how a value is quoted into a `key:value` token, how a token is
// added to a query, and the two right-click entries that add one. The
// grammar itself is `datalib_query` on the backend; the unified grid's
// search sends it to `/applet/unified_index`, the run log's to `/api/log`.
import type { DefaultMenuItem, MenuItemDef } from "ag-grid-community";

/// `value` as one token: bare when it can be, double-quoted otherwise.
/// Mirrors `datalib_query::quote` (`\"` and `\\` escape inside quotes).
export function quoteValue(v: string): string {
  const needsQuotes = v === "" || /[\s:"]/.test(v) || v.startsWith("-");
  if (!needsQuotes) return v;
  const escaped = v.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `"${escaped}"`;
}

export function filterToken(
  key: string,
  value: string,
  exclude: boolean,
): string {
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

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/// "Keep only" and "Exclude all" for one cell: each hands `apply` the
/// token it stands for. `shown` is the value as the cell displays it,
/// for the label; `value` is what the filter compares.
export function keepExcludeItems<T>(opts: {
  header: string;
  key: string;
  value: string;
  shown?: string;
  apply: (token: string) => void;
}): (MenuItemDef<T> | DefaultMenuItem)[] {
  const shown = opts.shown ?? opts.value;
  return [
    {
      name: `Keep only ${opts.header}=${shown}`,
      action: () => opts.apply(filterToken(opts.key, opts.value, false)),
    },
    {
      name: `Exclude all ${opts.header}=${shown}`,
      action: () => opts.apply(filterToken(opts.key, opts.value, true)),
    },
    "separator",
  ];
}
