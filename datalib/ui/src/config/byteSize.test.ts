import { describe, expect, it } from "vitest";

import { formatBytes, joinBytes, parseByteSize, splitBytes } from "./byteSize";

describe("parseByteSize", () => {
  it("reads every spelling the backend does", () => {
    expect(parseByteSize("250")?.bytes).toBe(250);
    expect(parseByteSize("5_000_000")?.bytes).toBe(5_000_000);
    expect(parseByteSize("5 MB")?.bytes).toBe(5_000_000);
    expect(parseByteSize("5MB")?.bytes).toBe(5_000_000);
    expect(parseByteSize("5mb")?.bytes).toBe(5_000_000);
    expect(parseByteSize("5 M")?.bytes).toBe(5_000_000);
    expect(parseByteSize(" 1.5 GB ")?.bytes).toBe(1_500_000_000);
    expect(parseByteSize("512 MiB")?.bytes).toBe(512 * 1024 * 1024);
    expect(parseByteSize("2 TB")?.bytes).toBe(2_000_000_000_000);
    expect(parseByteSize(5_000_000)?.bytes).toBe(5_000_000);
  });

  it("keeps the unit as written when the control offers it", () => {
    expect(parseByteSize("5000 KB")).toEqual({ bytes: 5_000_000, unit: "KB", amount: 5000 });
    expect(parseByteSize("5 mb")?.unit).toBe("MB");
    expect(parseByteSize("512 MiB")?.unit).toBeNull();
    expect(parseByteSize("250")?.unit).toBeNull();
  });

  it("refuses what it cannot read", () => {
    for (const bad of ["", "MB", "5 XB", "5 MBs", "five MB", "1.2.3 KB", "-5 MB"]) {
      expect(parseByteSize(bad), bad).toBeNull();
    }
  });
});

describe("formatBytes", () => {
  it("writes the whole-number form, and parses back to the same bytes", () => {
    expect(formatBytes(5_000_000)).toBe("5 MB");
    expect(formatBytes(250)).toBe("250 B");
    for (const bytes of [1, 250, 250_000, 5_000_000, 5_242_880]) {
      expect(parseByteSize(formatBytes(bytes))?.bytes).toBe(bytes);
    }
  });
});

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
