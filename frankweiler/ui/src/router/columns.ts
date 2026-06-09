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
//   /grid/card:q=foo&js=<sha256>           → [grid, card(q=foo,js=hash)]
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
  | {
      kind: "doc";
      md: string;
      /**
       * Optional anchor inside `md` (a `data-section-uuid` value) to
       * scroll-and-highlight on load. Populated when the user navigates
       * via a doc-to-doc edge: clicking a text-span source opens the
       * destination doc with the matching span pre-selected. Null /
       * absent when the column was opened from a grid row (selection
       * is computed dynamically in that path).
       */
      anchor?: string | null;
    }
  | {
      kind: "card";
      /** Search query that supplies the card's `rows`. Empty = match-all. */
      q: string;
      /**
       * Content hash of the JS source stored on the backend. Null = a
       * blank card not yet saved (column opens in edit mode and stays
       * there until the user clicks Save).
       */
      js: string | null;
    };

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
  if (c.kind === "card") {
    const sp = new URLSearchParams();
    if (c.q) sp.set("q", c.q);
    if (c.js) sp.set("js", c.js);
    const s = sp.toString();
    return s ? `card:${s}` : "card";
  }
  // doc UUIDs are URL-safe (hex + hyphens), but defensively encode in
  // case some renderer ever emits a non-UUID markdown id.
  const sp = new URLSearchParams();
  sp.set("md", c.md);
  if (c.anchor) sp.set("a", c.anchor);
  // Backwards-compatible: when there's no anchor we emit the legacy
  // shape `doc:<md>` so older bookmarks still parse cleanly.
  if (!c.anchor) {
    return `doc:${encodeURIComponent(c.md)}`;
  }
  return `doc:${sp.toString()}`;
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
  if (kind === "card") {
    const sp = new URLSearchParams(rest);
    return {
      kind: "card",
      q: sp.get("q") ?? "",
      js: sp.get("js"),
    };
  }
  if (kind === "doc") {
    // Two-shape decoder: legacy `doc:<md>` (no `=`) and current
    // `doc:md=…&a=…`. The presence of `md=` is the discriminator —
    // a bare UUID never contains it. When there's no anchor we omit
    // the field entirely (keep the column shape minimal so tests
    // and old callers stay simple).
    if (rest.includes("md=")) {
      const sp = new URLSearchParams(rest);
      const md = sp.get("md") ?? "";
      if (md.length === 0) return null;
      const anchor = sp.get("a");
      return anchor ? { kind: "doc", md, anchor } : { kind: "doc", md };
    }
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
    return a.md === b.md && (a.anchor ?? null) === (b.anchor ?? null);
  }
  if (a.kind === "card" && b.kind === "card") {
    return a.q === b.q && a.js === b.js;
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
