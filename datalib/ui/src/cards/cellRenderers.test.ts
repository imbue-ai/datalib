import { describe, expect, it } from "vitest";
import { windowPhrase } from "./cellRenderers";

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
