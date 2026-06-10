// URL codec for the miller view. The path is a /-separated list of
// `code:state` segments, one per column: `code` is the card source,
// `state` the card's opaque state string. Both are percent-escaped so
// embedded `/` and `:` survive; the first raw `:` in a segment is the
// separator. A column with empty state serializes as bare `code` —
// "code" and "code:" are equivalent.

export type ColumnSpec = { code: string; state: string };

export function encodeColumns(cols: ColumnSpec[]): string {
  return (
    "/" +
    cols
      .map(
        (c) =>
          encodeURIComponent(c.code) +
          (c.state ? ":" + encodeURIComponent(c.state) : ""),
      )
      .join("/")
  );
}

export function decodeColumns(path: string): ColumnSpec[] {
  return path
    .split("/")
    .filter((seg) => seg.length > 0)
    .map((seg) => {
      const i = seg.indexOf(":");
      const code = i === -1 ? seg : seg.slice(0, i);
      const state = i === -1 ? "" : seg.slice(i + 1);
      return { code: tryDecode(code), state: tryDecode(state) };
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
