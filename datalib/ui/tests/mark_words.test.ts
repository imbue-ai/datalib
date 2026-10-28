import { describe, expect, it } from "vitest";
import { markWords } from "../src/cards/chatSections.js";

function section(html: string): HTMLElement {
  const el = document.createElement("div");
  el.innerHTML = html;
  return el;
}

const marked = (el: HTMLElement) =>
  Array.from(el.querySelectorAll("mark.search-hit")).map((m) => m.textContent);

/// What a search result's link asks the page to find: the phrase where it
/// stands whole, else each word, never a button's label, and nothing left
/// from the search before.
describe("markWords", () => {
  it("marks the words as a phrase where they stand together", () => {
    const el = section("<p>Worf: Warp core breach in two minutes. The warp field holds.</p>");
    const first = markWords(el, "warp core breach");
    expect(marked(el)).toEqual(["Warp core breach"]);
    expect(first?.textContent).toBe("Warp core breach");
  });

  it("marks each word where the phrase is not there", () => {
    const el = section("<p>The <b>warp</b> field and the core</p><button>warp</button>");
    const first = markWords(el, "warp core breach");
    expect(marked(el)).toEqual(["warp", "core"]);
    expect(first?.textContent).toBe("warp");
  });

  it("leaves words of under three letters unmarked, and clears the last search", () => {
    const el = section("<p>Data to the bridge, to the bridge</p>");
    markWords(el, "bridge");
    expect(marked(el)).toEqual(["bridge", "bridge"]);
    markWords(el, "to data");
    expect(marked(el)).toEqual(["Data"]);
    expect(el.textContent).toBe("Data to the bridge, to the bridge");
  });

  it("finds nothing to mark without words", () => {
    const el = section("<p>Engage</p>");
    expect(markWords(el, "  ")).toBeNull();
    expect(markWords(el, "tea")).toBeNull();
  });
});
