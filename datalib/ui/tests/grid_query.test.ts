import { describe, expect, it } from "vitest";
import { plainWords, replaceToken, searchDelay, tokenValue, withToken } from "../src/grid/query";

describe("replaceToken", () => {
  it("puts the token where the key's word was, and keeps the rest", () => {
    expect(
      replaceToken("process:http min_level:info thread:main", "min_level", "min_level:warn"),
    ).toBe("process:http min_level:warn thread:main");
  });

  it("appends when the key was absent, and drops every one of it when the token is null", () => {
    expect(replaceToken("process:http", "min_level", "min_level:warn")).toBe(
      "process:http min_level:warn",
    );
    expect(replaceToken("min_level:info a min_level:warn b", "min_level", null)).toBe("a b");
    expect(replaceToken("", "min_level", null)).toBe("");
  });

  it("leaves a negated word alone: that is a different filter", () => {
    expect(replaceToken("-min_level:warn", "min_level", "min_level:info")).toBe(
      "-min_level:warn min_level:info",
    );
  });
});

describe("tokenValue", () => {
  it("reads the first word with the key, or nothing", () => {
    expect(tokenValue("process:http min_level:warn", "min_level")).toBe("warn");
    expect(tokenValue("process:http", "min_level")).toBeNull();
  });
});

describe("withToken", () => {
  it("adds a word once", () => {
    expect(withToken("a", "b:c")).toBe("a b:c");
    expect(withToken("a b:c", "b:c")).toBe("a b:c");
  });
});

describe("plainWords", () => {
  it("is what is left once the filters are taken out", () => {
    expect(plainWords('warp author:"Jean-Luc Picard" -kind:email core is:do')).toBe("warp core");
    expect(plainWords('"earl grey" -tea')).toBe('"earl grey" -tea');
    expect(plainWords("before:2026-01 ")).toBe("");
  });
});

describe("searchDelay", () => {
  /// Each change to the plain words is a qmd search, seconds long and one
  /// at a time; at 150 ms every pause between keystrokes started another
  /// that the server finished even after the page had moved on.
  it("waits longer when the words qmd ranks changed", () => {
    expect(searchDelay("warp co", "warp cor", true)).toBe(600);
    expect(searchDelay("warp", "warp kind:em", true)).toBe(150);
    expect(searchDelay("warp kind:em", "warp kind:email", true)).toBe(150);
  });

  it("does not wait for words nothing ranks", () => {
    expect(searchDelay("warp co", "warp cor", false)).toBe(150);
    expect(searchDelay("warp", "", true)).toBe(150);
  });
});
