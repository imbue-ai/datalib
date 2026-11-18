import { describe, expect, it } from "vitest";
import { formatUnit, windowPhrase } from "./cellRenderers";

describe("windowPhrase", () => {
  /// The Size column's minutes and the Items column's days both read as
  /// words, in the largest unit that divides them evenly.
  it("names a window in the largest whole unit", () => {
    expect(windowPhrase(300)).toBe("the last 5 minutes");
    expect(windowPhrase(60)).toBe("the last minute");
    expect(windowPhrase(3 * 86_400)).toBe("the last 3 days");
    expect(windowPhrase(7_200)).toBe("the last 2 hours");
    expect(windowPhrase(90)).toBe("the last 90 seconds");
  });
});

describe("formatUnit", () => {
  /// The Modal spend column's cells are millionths of a dollar, and read
  /// as dollars, down past a cent for a small run.
  it("reads micro_usd as dollars", () => {
    expect(formatUnit(8_253_000, "micro_usd")).toBe("$8.25");
    expect(formatUnit(1_234, "micro_usd")).toBe("$0.0012");
    expect(formatUnit(48_210, "items")).toBe((48_210).toLocaleString());
  });
});
