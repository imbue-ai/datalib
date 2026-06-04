import { describe, it, expect } from "vitest";
import {
  type Column,
  emptyGrid,
  encodeColumn,
  decodeColumn,
  encodeStack,
  decodeStack,
  columnsEqual,
  stacksEqual,
} from "../src/router/columns";

describe("encodeColumn", () => {
  it("encodes a bare grid as `grid`", () => {
    expect(encodeColumn(emptyGrid())).toBe("grid");
  });

  it("encodes grid params as urlencoded form", () => {
    expect(
      encodeColumn({ kind: "grid", q: "treemap", sel: "abc-123", agCols: null }),
    ).toBe("grid:q=treemap&sel=abc-123");
  });

  it("percent-encodes special chars in the query", () => {
    expect(
      encodeColumn({
        kind: "grid",
        q: "hello world",
        sel: null,
        agCols: null,
      }),
    ).toBe("grid:q=hello+world");
    expect(
      encodeColumn({ kind: "grid", q: "a/b", sel: null, agCols: null }),
    ).toBe("grid:q=a%2Fb");
  });

  it("passes the AG Grid base64url state through unencoded", () => {
    const ag = "eyJhIjoxfQ"; // sample url-safe base64
    expect(
      encodeColumn({ kind: "grid", q: "", sel: null, agCols: ag }),
    ).toBe(`grid:ag=${ag}`);
  });

  it("encodes a doc as `doc:<uuid>`", () => {
    expect(encodeColumn({ kind: "doc", md: "abc-123" })).toBe("doc:abc-123");
  });
});

describe("decodeColumn", () => {
  it("decodes `grid` as an empty grid", () => {
    expect(decodeColumn("grid")).toEqual(emptyGrid());
  });

  it("decodes `grid:q=treemap&sel=abc`", () => {
    expect(decodeColumn("grid:q=treemap&sel=abc-123")).toEqual({
      kind: "grid",
      q: "treemap",
      sel: "abc-123",
      agCols: null,
    });
  });

  it("decodes percent-encoded query back to raw", () => {
    expect(decodeColumn("grid:q=hello+world")).toEqual({
      kind: "grid",
      q: "hello world",
      sel: null,
      agCols: null,
    });
    expect(decodeColumn("grid:q=a%2Fb")).toEqual({
      kind: "grid",
      q: "a/b",
      sel: null,
      agCols: null,
    });
  });

  it("reads `ag` back into the agCols slot", () => {
    expect(decodeColumn("grid:ag=eyJhIjoxfQ")).toEqual({
      kind: "grid",
      q: "",
      sel: null,
      agCols: "eyJhIjoxfQ",
    });
  });

  it("decodes `doc:<uuid>`", () => {
    expect(decodeColumn("doc:abc-123")).toEqual({ kind: "doc", md: "abc-123" });
  });

  it("returns null for unknown kinds", () => {
    expect(decodeColumn("unknown")).toBeNull();
    expect(decodeColumn("foo:bar=baz")).toBeNull();
  });

  it("returns null for an empty doc payload", () => {
    expect(decodeColumn("doc:")).toBeNull();
  });

  it("returns null for an empty segment", () => {
    expect(decodeColumn("")).toBeNull();
  });
});

describe("encodeStack / decodeStack round trip", () => {
  const cases: { name: string; stack: Column[]; path: string }[] = [
    { name: "empty", stack: [], path: "/" },
    { name: "just grid", stack: [emptyGrid()], path: "/grid" },
    {
      name: "grid + doc",
      stack: [emptyGrid(), { kind: "doc", md: "abc" }],
      path: "/grid/doc:abc",
    },
    {
      name: "doc only",
      stack: [{ kind: "doc", md: "abc" }],
      path: "/doc:abc",
    },
    {
      name: "doc chain",
      stack: [
        { kind: "doc", md: "abc" },
        { kind: "doc", md: "def" },
      ],
      path: "/doc:abc/doc:def",
    },
    {
      name: "grid with state + doc",
      stack: [
        { kind: "grid", q: "treemap", sel: "row-1", agCols: null },
        { kind: "doc", md: "def" },
      ],
      path: "/grid:q=treemap&sel=row-1/doc:def",
    },
  ];

  for (const c of cases) {
    it(`round-trips: ${c.name}`, () => {
      expect(encodeStack(c.stack)).toBe(c.path);
      expect(decodeStack(c.path)).toEqual(c.stack);
    });
  }

  it("decodeStack tolerates leading/trailing slashes + empty segments", () => {
    expect(decodeStack("///grid//doc:abc//")).toEqual([
      emptyGrid(),
      { kind: "doc", md: "abc" },
    ]);
  });

  it("decodeStack drops unknown segments instead of throwing", () => {
    expect(decodeStack("/grid/garbage/doc:abc")).toEqual([
      emptyGrid(),
      { kind: "doc", md: "abc" },
    ]);
  });
});

describe("columnsEqual / stacksEqual", () => {
  it("treats matching grid state as equal", () => {
    expect(
      columnsEqual(
        { kind: "grid", q: "x", sel: null, agCols: null },
        { kind: "grid", q: "x", sel: null, agCols: null },
      ),
    ).toBe(true);
  });

  it("treats different grid state as unequal", () => {
    expect(
      columnsEqual(
        { kind: "grid", q: "x", sel: null, agCols: null },
        { kind: "grid", q: "y", sel: null, agCols: null },
      ),
    ).toBe(false);
  });

  it("treats different kinds as unequal", () => {
    expect(
      columnsEqual(emptyGrid(), { kind: "doc", md: "abc" }),
    ).toBe(false);
  });

  it("compares stacks element-wise", () => {
    expect(stacksEqual([emptyGrid()], [emptyGrid()])).toBe(true);
    expect(stacksEqual([], [])).toBe(true);
    expect(stacksEqual([emptyGrid()], [])).toBe(false);
    expect(
      stacksEqual(
        [emptyGrid(), { kind: "doc", md: "abc" }],
        [emptyGrid(), { kind: "doc", md: "abc" }],
      ),
    ).toBe(true);
    expect(
      stacksEqual(
        [emptyGrid(), { kind: "doc", md: "abc" }],
        [emptyGrid(), { kind: "doc", md: "xyz" }],
      ),
    ).toBe(false);
  });
});
