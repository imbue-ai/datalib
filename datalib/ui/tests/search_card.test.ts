import { describe, expect, it } from "vitest";
import {
  decodeSearchState,
  encodeSearchState,
  freeText,
  markWords,
  searchQuery,
} from "../src/cards/search";

const input = (text: string, meaningOnly = false, sourceId: string | null = null) => ({
  text,
  meaningOnly,
  sourceId,
});

describe("searchQuery", () => {
  it("browses every document when nothing is typed", () => {
    expect(searchQuery(input(""))).toBe("is:document");
  });

  it("sends typed text as it is", () => {
    expect(searchQuery(input("risa trip"))).toBe("risa trip");
  });

  /** "Meaning only" must move only the free text; a filter inside the predicate would be searched as words. */
  it("moves only the free text into a meaning-only predicate", () => {
    expect(searchQuery(input("author:worf risa trip", true))).toBe(
      'author:worf qmd_vsearch:"risa trip"',
    );
  });

  it("keeps a query of filters alone, with no default added", () => {
    expect(searchQuery(input("-kind:contact before:2371-01-01"))).toBe(
      "-kind:contact before:2371-01-01",
    );
  });

  it("adds the picked source, and leaves it out when asked", () => {
    expect(searchQuery(input("risa", false, "slack"))).toBe("risa source_id:slack");
    expect(searchQuery(input("risa", false, "slack"), false)).toBe("risa");
  });
});

describe("the search state string", () => {
  it("round-trips", () => {
    const s = input("warp core", true, "tng_email");
    expect(decodeSearchState(encodeSearchState(s), "")).toEqual(s);
  });
  it("starts from the source's own query when it has none", () => {
    expect(decodeSearchState("", "kraken")).toEqual(input("kraken"));
  });
});

describe("markWords", () => {
  it("marks whole words of the free text, case-blind", () => {
    expect(markWords("Off to Risa, not Risan", "author:x risa")).toEqual([
      { text: "Off to ", hit: false },
      { text: "Risa", hit: true },
      { text: ", not Risan", hit: false },
    ]);
  });
  it("marks nothing when only filters were typed", () => {
    expect(markWords("anything", "kind:chat")).toEqual([{ text: "anything", hit: false }]);
  });
  it("finds free text among filter words", () => {
    expect(freeText("kind:chat  warp   -author:q core")).toBe("warp core");
  });
});
