import { describe, expect, it } from "vitest";

import { joinBytes, splitBytes } from "./byteSize";

describe("splitBytes", () => {
  it("picks the largest unit that leaves a whole number", () => {
    expect(splitBytes(5_000_000)).toEqual({ amount: 5, unit: "MB" });
    expect(splitBytes(250_000)).toEqual({ amount: 250, unit: "KB" });
    expect(splitBytes(2_000_000_000)).toEqual({ amount: 2, unit: "GB" });
    expect(splitBytes(1_500_000)).toEqual({ amount: 1500, unit: "KB" });
  });

  // A hand-edited binary value has no clean decimal form; showing it in
  // bytes is honest, showing "5.24288 MB" is not.
  it("falls back to bytes for a value no unit divides", () => {
    expect(splitBytes(5_242_880)).toEqual({ amount: 5_242_880, unit: "B" });
    expect(splitBytes(250)).toEqual({ amount: 250, unit: "B" });
  });

  it("starts an empty or zero value on the default unit", () => {
    expect(splitBytes(0).unit).toBe("KB");
    expect(splitBytes(NaN).unit).toBe("KB");
  });
});

describe("joinBytes", () => {
  it("round-trips splitBytes", () => {
    for (const bytes of [1, 250, 250_000, 5_000_000, 5_242_880, 3_000_000_000]) {
      const { amount, unit } = splitBytes(bytes);
      expect(joinBytes(amount, unit)).toBe(bytes);
    }
  });

  it("writes an integer even for a fractional amount", () => {
    expect(joinBytes(1.5, "MB")).toBe(1_500_000);
    expect(joinBytes(0.3, "KB")).toBe(300);
  });
});
