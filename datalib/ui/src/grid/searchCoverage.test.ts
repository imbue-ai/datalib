import { describe, expect, it } from "vitest";
import { searchCoverage } from "./searchCoverage";

const summary = (documents: number, embedded: number) => ({ documents, embedded });

describe("searchCoverage", () => {
  /// A root no sync has built an index for read "0 of 0 documents
  /// searchable", which says nothing about what to do (#901).
  it("says the index is not built yet, and how to build it", () => {
    const c = searchCoverage({ index_present: false, summary: summary(0, 0) });
    expect(c.text).toBe("search index not built yet — sync to build it");
  });

  it("does not call an index that would not open unbuilt", () => {
    const c = searchCoverage({
      index_present: false,
      summary: summary(0, 0),
      errors: ["qmd index: database is locked"],
    });
    expect(c.text).toBe("search index could not be read");
  });

  /// Keyword search reaches every indexed document before any is
  /// embedded; this state read "0 of 51 documents searchable" (#901).
  it("counts keyword and semantic coverage apart", () => {
    const c = searchCoverage({ index_present: true, summary: summary(51, 0) });
    expect(c.text).toBe("51 documents searchable · 0 with semantic search");
    expect(c.title).toContain("51 are still waiting on embeddings");
  });

  it("says when every document is embedded", () => {
    const c = searchCoverage({ index_present: true, summary: summary(51, 51) });
    expect(c.text).toBe("51 documents searchable · 51 with semantic search");
    expect(c.title).toContain("both reach all of them");
  });
});
