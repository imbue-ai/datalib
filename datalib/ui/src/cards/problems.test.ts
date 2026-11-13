import { describe, expect, it } from "vitest";
import { severityWord } from "./problems";

describe("severityWord", () => {
  /** The banner said "WARNING" while the problems table said "WARN". */
  it("writes a severity as the problems table does", () => {
    expect(["error", "warning", "info"].map(severityWord)).toEqual(["ERROR", "WARN", "INFO"]);
    expect(severityWord("catastrophic")).toBe("catastrophic");
  });
});
