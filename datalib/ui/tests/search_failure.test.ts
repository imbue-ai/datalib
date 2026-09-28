import { describe, expect, it } from "vitest";
import { ApiError, errorDetail } from "@/apiError";
import { searchFailure } from "@/cards/searchFailure";

const URL = "/applet/unified_index/search?q=is%3Adocument&limit=100000";

describe("searchFailure", () => {
  /// The gateway's timeout used to reach the grid as the raw URL, the
  /// status and a JSON body ending in "os error 35".
  it("says a 504 timed out, in the gateway's own words", () => {
    const e = new ApiError(URL, 504, 'applet "unified_index" did not answer within 30s');
    expect(searchFailure(e)).toEqual({
      message: 'Search timed out: applet "unified_index" did not answer within 30s.',
      detail: `${URL} → 504: applet "unified_index" did not answer within 30s`,
    });
  });
  it("says any other status failed, and falls back to the status", () => {
    expect(searchFailure(new ApiError(URL, 502, 'no applet "x".')).message).toBe(
      'Search failed: no applet "x".',
    );
    expect(searchFailure(new ApiError(URL, 500, "")).message).toBe(
      "Search failed: the server answered 500.",
    );
  });
  it("says the server could not be reached when fetch never got an answer", () => {
    expect(searchFailure(new TypeError("Load failed"))).toEqual({
      message: "Search failed: the server could not be reached.",
      detail: "Load failed",
    });
  });
});

describe("errorDetail", () => {
  it("takes the sentence out of a gateway error body", () => {
    expect(errorDetail('{"error":"read response: boom"}\n')).toBe("read response: boom");
  });
  it("keeps any other body as its text", () => {
    expect(errorDetail(" plain words ")).toBe("plain words");
    expect(errorDetail('{"other":1}')).toBe('{"other":1}');
  });
});
