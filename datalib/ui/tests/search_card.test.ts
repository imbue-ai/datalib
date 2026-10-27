import { describe, expect, it } from "vitest";
import { freeText, markWords, pickedSource, searchTitle, setSource } from "../src/cards/search";

describe("the picked source, as the query says it", () => {
  it("is the one source_id filter, typed or clicked", () => {
    expect(pickedSource("risa source_id:slack")).toBe("slack");
    expect(pickedSource('source_id:"tng email"')).toBe("tng email");
    expect(pickedSource("risa")).toBeNull();
  });

  it("is none when the query names several, or only excludes one", () => {
    expect(pickedSource("source_id:slack source_id:tng_email")).toBeNull();
    expect(pickedSource("-source_id:slack")).toBeNull();
  });

  it("replaces every source filter and leaves the rest of the query", () => {
    expect(setSource("source_id:slack risa source_id:tng_email", "notion")).toBe(
      "risa source_id:notion",
    );
    expect(setSource("risa source_id:slack -source_id:garmin", null)).toBe(
      "risa -source_id:garmin",
    );
    expect(setSource("", "slack")).toBe("source_id:slack");
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
  it("marks the words a meaning-only predicate carries", () => {
    expect(markWords("warp core", 'qmd_vsearch:"warp core"').filter((p) => p.hit)).toHaveLength(2);
  });
  it("finds free text among filter words", () => {
    expect(freeText("kind:chat  warp   -author:q core")).toBe("warp core");
  });
});

describe("a search's tab name", () => {
  const names: Record<string, string> = {
    "contact:c1": "William T. Riker",
    "email:riker@enterprise.starfleet": "Will Riker",
  };
  const nameOf = (value: string) => names[value] ?? null;

  /** "Everything with" titled its tab `Search: with:contact:6277af2c-…`. */
  it("calls a person by their name, and everything with one of them so", () => {
    expect(searchTitle("with:contact:c1", nameOf)).toBe("Everything with William T. Riker");
    expect(searchTitle("from:email:riker@enterprise.starfleet is:document", nameOf)).toBe(
      "Search: from:Will Riker is:document",
    );
    expect(searchTitle("with:contact:c2", nameOf)).toBe("Search: with:contact:c2");
    expect(searchTitle("warp core", nameOf)).toBe("Search: warp core");
    const day = (v: string) => (v === "2369-04-15" ? "Tue, Apr 15, 2369" : null);
    expect(searchTitle("after:2369-04-15 risa", day)).toBe("Search: after:Tue, Apr 15, 2369 risa");
  });
});
