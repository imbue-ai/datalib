// URL codec for the miller view. The path is a /-separated list of
// `code:size:state` segments, one per column:
//   - `code` is the card source,
//   - `size` is the column's width as a ratio of the default width
//     (e.g. `0.5`, `1.12`), or empty when the column is at the default
//     width,
//   - `state` is the card's opaque state string.
// `code` and `state` are percent-escaped (so embedded `/` and `:`
// survive — `encodeURIComponent` escapes both); `size` is a plain
// number, never containing `:`. So a segment splits cleanly on `:`.
//
// Trailing empties are dropped, so the common cases stay terse:
//   `code`              — default width, no state
//   `code:1.2`          — resized, no state
//   `code::abc`         — default width, state "abc"
//   `code:1.2:abc`      — resized, with state
// "code", "code:" and "code::" are all equivalent (default width, no
// state).

export type ColumnSpec = {
  code: string;
  // Width ratio vs. the default column width, or null/undefined for the
  // default width. Producers round to two decimals.
  size?: number | null;
  state: string;
};

export function encodeColumns(cols: ColumnSpec[]): string {
  return "/" + cols.map(encodeSegment).join("/");
}

function encodeSegment(c: ColumnSpec): string {
  const code = encodeURIComponent(c.code);
  const size = c.size != null ? String(c.size) : "";
  const state = c.state ? encodeURIComponent(c.state) : "";
  // Emit only as many `:`-joined fields as needed; a set state forces
  // the (possibly empty) size slot to keep its position.
  if (state) return `${code}:${size}:${state}`;
  if (size) return `${code}:${size}`;
  return code;
}

export function decodeColumns(path: string): ColumnSpec[] {
  return path
    .split("/")
    .filter((seg) => seg.length > 0)
    .map((seg) => {
      const [codeRaw, sizeRaw = "", stateRaw = ""] = seg.split(":");
      const n = sizeRaw ? Number.parseFloat(sizeRaw) : NaN;
      const size = Number.isFinite(n) && n > 0 ? n : null;
      return { code: tryDecode(codeRaw), size, state: tryDecode(stateRaw) };
    })
    .filter((c) => c.code.length > 0);
}

// Malformed percent-escapes in a hand-edited URL shouldn't crash the
// whole view; take the raw text instead.
function tryDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}
