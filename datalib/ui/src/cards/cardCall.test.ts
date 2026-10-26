import { describe, expect, it } from "vitest";
import {
  CardSourceError,
  componentName,
  formatCardCall,
  lookupFunction,
  parseCardSource,
} from "./cardCall";

describe("parseCardSource", () => {
  it("reads a call with no arguments", () => {
    expect(parseCardSource("searchView()")).toEqual({ name: "searchView", args: [] });
    expect(parseCardSource("  comp.user.tetris ( )  ")).toEqual({
      name: "comp.user.tetris",
      args: [],
    });
  });

  it("reads strict JSON arguments, as every builder writes them", () => {
    expect(parseCardSource('documentView("md-1", null)')).toEqual({
      name: "documentView",
      args: ["md-1", null],
    });
    expect(parseCardSource('personView("email:a@b.c", {"seenIn":null})').args).toEqual([
      "email:a@b.c",
      { seenIn: null },
    ]);
  });

  it("reads JSON5: unquoted keys, single quotes, trailing commas", () => {
    expect(parseCardSource("gridView({ q: 'picard', columns: ['kind',], },)").args).toEqual([
      { q: "picard", columns: ["kind"] },
    ]);
  });

  it("keeps parentheses and brackets inside strings", () => {
    expect(parseCardSource('searchView({"q": "a) b] (c"})').args).toEqual([{ q: "a) b] (c" }]);
  });

  it("round-trips what formatCardCall writes", () => {
    const args = [{ url: "/api/manage/rows", n: 3, ok: true }, "x"];
    expect(parseCardSource(formatCardCall("tableView", args))).toEqual({ name: "tableView", args });
  });

  /** The 2026-10-19 hole: a URL path segment holding JavaScript ran with `new Function`. */
  it.each([
    ["an arrow function", '(root) => { root.textContent = "hi" }'],
    ["a call on a call's result", "gridView().constructor"],
    ["two statements", "gridView(); fetch('/api/config')"],
    ["a call followed by more code", "gridView() + fetch('/api/config')"],
    ["a nested call", "documentView(fetch('/api/config'))"],
    ["an identifier argument", "gridView(window)"],
    ["a template literal", "searchView(`${document.cookie}`)"],
    ["an argument that closes the list", "gridView(1], [2)"],
    ["a bare name", "gridView"],
    ["an empty string", ""],
  ])("refuses %s", (_what, source) => {
    expect(() => parseCardSource(source)).toThrow(CardSourceError);
  });
});

describe("componentName", () => {
  it("splits comp.<ns>.<name> and nothing else", () => {
    expect(componentName("comp.user.tetris")).toEqual({ ns: "user", name: "tetris" });
    expect(componentName("gridView")).toBeNull();
    expect(componentName("comp.user")).toBeNull();
    expect(componentName("comp.user.a.b")).toBeNull();
    expect(componentName("x.user.tetris")).toBeNull();
  });
});

describe("lookupFunction", () => {
  const table = { gridView: () => "grid", notAFunction: 3 };

  it("finds an own function", () => {
    expect(lookupFunction(table, "gridView")?.()).toBe("grid");
  });

  it("finds nothing the table does not own", () => {
    for (const key of ["constructor", "toString", "__proto__", "hasOwnProperty", "notAFunction"]) {
      expect(lookupFunction(table, key), key).toBeNull();
    }
  });
});
