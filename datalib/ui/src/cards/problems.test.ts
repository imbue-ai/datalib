import { describe, expect, it } from "vitest";
import { problemLabel, severityWord } from "./problems";

describe("severityWord", () => {
  /** The banner said "WARNING" while the problems table said "WARN". */
  it("writes a severity as the problems table does", () => {
    expect(["error", "warning", "info"].map(severityWord)).toEqual(["ERROR", "WARN", "INFO"]);
    expect(severityWord("catastrophic")).toBe("catastrophic");
  });
});

describe("problemLabel", () => {
  /** A setting `conv_uuids` left with nothing to do read as its raw reason word. */
  it("says an ignored setting does nothing", () => {
    expect(problemLabel({ field: "since", reason: "ignored", rule: null })).toBe(
      "`since` is set, but another setting takes its place, so it does nothing",
    );
  });
});
