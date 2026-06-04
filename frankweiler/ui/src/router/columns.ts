// URL-path encoding for the Miller column stack.
//
// Each column is one path segment shaped `kind` or `kind:k=v&k=v`.
// The whole stack is just the segments joined by `/`. Empty path = no
// columns. Examples:
//
//   /grid                                  → [grid]
//   /grid:q=treemap&sel=abc                → [grid?q=treemap,sel=abc]
//   /grid/doc:abc                          → [grid, doc:abc]
//   /doc:abc/doc:def                       → [doc:abc, doc:def]
//
// Per-column params are URL-form-encoded: spaces / slashes inside a
// search query become `+` / `%2F`; the AG Grid column-state blob is
// already base64-url-safe and round-trips unchanged.

export type GridColumnState = {
  /** Search query. Empty / missing = unset. */
  q: string;
  /** Selected grid row uuid. Null = no selection. */
  sel: string | null;
  /** Base64url-packed AG Grid column state. Null = grid defaults. */
  agCols: string | null;
};

export type Column =
  | ({ kind: "grid" } & GridColumnState)
  | { kind: "doc"; md: string };

/** Empty grid column with no state. The natural default. */
export function emptyGrid(): Column {
  return { kind: "grid", q: "", sel: null, agCols: null };
}

export function encodeColumn(c: Column): string {
  if (c.kind === "grid") {
    const sp = new URLSearchParams();
    if (c.q) sp.set("q", c.q);
    if (c.sel) sp.set("sel", c.sel);
    if (c.agCols) sp.set("ag", c.agCols);
    const s = sp.toString();
    return s ? `grid:${s}` : "grid";
  }
  // doc UUIDs are URL-safe (hex + hyphens), but defensively encode in
  // case some renderer ever emits a non-UUID markdown id.
  return `doc:${encodeURIComponent(c.md)}`;
}

export function decodeColumn(segment: string): Column | null {
  if (segment.length === 0) return null;
  const colon = segment.indexOf(":");
  const kind = colon === -1 ? segment : segment.slice(0, colon);
  const rest = colon === -1 ? "" : segment.slice(colon + 1);

  if (kind === "grid") {
    const sp = new URLSearchParams(rest);
    return {
      kind: "grid",
      q: sp.get("q") ?? "",
      sel: sp.get("sel"),
      agCols: sp.get("ag"),
    };
  }
  if (kind === "doc") {
    const md = decodeURIComponent(rest);
    if (md.length === 0) return null;
    return { kind: "doc", md };
  }
  return null;
}

/** Encode a stack as a leading-slash path. Empty stack → "/". */
export function encodeStack(cols: Column[]): string {
  if (cols.length === 0) return "/";
  return "/" + cols.map(encodeColumn).join("/");
}

/**
 * Parse a path back into a stack. Skips unknown / malformed segments
 * silently — the URL is partly user-editable, so we want best-effort
 * tolerance instead of throwing.
 */
export function decodeStack(path: string): Column[] {
  const out: Column[] = [];
  for (const seg of path.split("/")) {
    if (seg.length === 0) continue;
    const c = decodeColumn(seg);
    if (c) out.push(c);
  }
  return out;
}

export function columnsEqual(a: Column, b: Column): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "grid" && b.kind === "grid") {
    return a.q === b.q && a.sel === b.sel && a.agCols === b.agCols;
  }
  if (a.kind === "doc" && b.kind === "doc") {
    return a.md === b.md;
  }
  return false;
}

export function stacksEqual(a: Column[], b: Column[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (!columnsEqual(a[i], b[i])) return false;
  }
  return true;
}
