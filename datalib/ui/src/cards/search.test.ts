import { describe, expect, it } from "vitest";
import {
  DEFAULT_CUTOFF,
  isOneRowPerDocument,
  semanticOf,
  setOneRowPerDocument,
  setSemantic,
} from "./search";

describe("semantic search in a query", () => {
  it("is on at the default with nothing written", () => {
    expect(semanticOf("warp core")).toBe(DEFAULT_CUTOFF);
    expect(semanticOf("warp core semantic:on")).toBe(DEFAULT_CUTOFF);
    expect(semanticOf("")).toBe(DEFAULT_CUTOFF);
  });

  it("reads off and a cutoff, the last one winning as on the server", () => {
    expect(semanticOf("warp semantic:off")).toBeNull();
    expect(semanticOf("semantic:0.4 warp")).toBe(0.4);
    expect(semanticOf("semantic:off semantic:0.3")).toBe(0.3);
    expect(semanticOf("semantic:0")).toBe(0);
  });

  it("writes the default as nothing, so a plain search stays plain", () => {
    expect(setSemantic("warp core", DEFAULT_CUTOFF)).toBe("warp core");
    expect(setSemantic("warp core semantic:0.4", DEFAULT_CUTOFF)).toBe("warp core");
    expect(setSemantic("warp core semantic:off", DEFAULT_CUTOFF)).toBe("warp core");
  });

  it("replaces what was there, and leaves the rest of the query", () => {
    expect(setSemantic("warp core", null)).toBe("warp core semantic:off");
    expect(setSemantic("warp semantic:off from:riker", 0.4)).toBe("warp from:riker semantic:0.4");
    expect(setSemantic("warp semantic:0.3 semantic:0.5", 0.4)).toBe("warp semantic:0.4");
  });

  it("reads back what it writes", () => {
    for (const cutoff of [null, 0, 0.3, 0.45, DEFAULT_CUTOFF]) {
      expect(semanticOf(setSemantic("warp", cutoff))).toBe(cutoff);
    }
  });
});

describe("one row per document in a query", () => {
  it("is the is:document term", () => {
    expect(isOneRowPerDocument("warp is:document")).toBe(true);
    expect(isOneRowPerDocument("warp -is:document")).toBe(false);
    expect(isOneRowPerDocument("warp")).toBe(false);
  });

  it("adds it once and takes it away, leaving the opposite alone", () => {
    expect(setOneRowPerDocument("warp", true)).toBe("warp is:document");
    expect(setOneRowPerDocument("warp is:document", true)).toBe("warp is:document");
    expect(setOneRowPerDocument("warp is:document from:riker", false)).toBe("warp from:riker");
    expect(setOneRowPerDocument("warp -is:document", false)).toBe("warp -is:document");
  });
});
