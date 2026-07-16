import { describe, expect, it } from "vitest";
import { displayTitle, titled } from "../src/cards/title";
import type { CardRender } from "../src/cards/types";

describe("titled", () => {
  it("attaches the title to the render function", () => {
    const render: CardRender = () => () => {};
    expect(titled("Search", render)).toBe(render);
    expect(render.cardTitle).toBe("Search");
  });
});

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
