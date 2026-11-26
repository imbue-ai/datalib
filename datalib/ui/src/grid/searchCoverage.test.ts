import { describe, expect, it } from "vitest";
import { searchNotices } from "./searchCoverage";

describe("the lines above a search's results", () => {
  it("says nothing when the search reached everything", () => {
    expect(searchNotices(null)).toEqual([]);
    expect(searchNotices({})).toEqual([]);
    expect(searchNotices({ partial: null })).toEqual([]);
  });

  it("says what part of the search could not answer", () => {
    expect(searchNotices({ partial: "Part of this search could not answer" })).toEqual([
      { text: "Part of this search could not answer" },
    ]);
  });
});
