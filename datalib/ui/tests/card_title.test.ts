import { describe, expect, it } from "vitest";
import { displayTitle } from "../src/cards/title";

describe("displayTitle", () => {
  it("prefers the declared title", () => {
    expect(displayTitle("gridView()", "Search")).toBe("Search");
  });
  it("falls back to the factory/alias name for name(...) source", () => {
    expect(displayTitle('myWidget({ q: "x" })', null)).toBe("myWidget");
    expect(displayTitle("  spaced ()", undefined)).toBe("spaced");
  });
  it("labels blank source as a new card", () => {
    expect(displayTitle("   ", null)).toBe("new card");
  });
  it("labels other expressions generically", () => {
    expect(displayTitle('(root) => { root.textContent = "hi" }', null)).toBe(
      "custom card",
    );
  });
});
