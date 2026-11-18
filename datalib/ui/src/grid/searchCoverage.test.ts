import { describe, expect, it } from "vitest";
import { searchNotices } from "./searchCoverage";

describe("the lines above a search's results", () => {
  it("says nothing when the search reached everything", () => {
    expect(searchNotices(null)).toEqual([]);
    expect(searchNotices({})).toEqual([]);
    expect(searchNotices({ partial: null, without_vectors: null })).toEqual([]);
    expect(searchNotices({ without_vectors: { total: 0, by_source: [] } })).toEqual([]);
  });

  it("counts the candidates without vectors, and names their sources on hover", () => {
    expect(
      searchNotices({
        without_vectors: {
          total: 1511,
          by_source: [
            ["notion", 1500],
            ["slack", 11],
          ],
        },
      }),
    ).toEqual([
      {
        text: "1,511 candidates lack semantic embedding vectors",
        title: "Search by meaning cannot reach these yet: notion 1,500, slack 11",
      },
    ]);
  });

  it("reads right for one", () => {
    const [one] = searchNotices({ without_vectors: { total: 1, by_source: [["notion", 1]] } });
    expect(one.text).toBe("1 candidate lacks semantic embedding vectors");
  });

  it("says first what part of the search could not answer", () => {
    const notices = searchNotices({
      partial: "Part of this search could not answer",
      without_vectors: { total: 2, by_source: [["slack", 2]] },
    });
    expect(notices.map((n) => n.text)).toEqual([
      "Part of this search could not answer",
      "2 candidates lack semantic embedding vectors",
    ]);
  });
});
