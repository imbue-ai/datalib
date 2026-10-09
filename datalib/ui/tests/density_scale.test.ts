import { describe, expect, it } from "vitest";
import { MAX_STEP, MIN_STEP, STEPS, onScale, stepIndex } from "@/densityScale";

describe("the size scale", () => {
  it("puts a stored value on an eighth, in range", () => {
    expect(onScale("1")).toBe(1);
    expect(onScale("0.6")).toBe(0.625);
    expect(onScale(9)).toBe(MAX_STEP);
    expect(onScale(-1)).toBe(MIN_STEP);
  });

  /** A value an older build stored ("compact", "comfortable") is not a step, and starts at the smallest. */
  it("starts anything that is not a number at the smallest step", () => {
    expect(onScale("comfortable")).toBe(MIN_STEP);
    expect(onScale(null)).toBe(MIN_STEP);
  });

  it("counts nine steps, the first 0 and the last 8", () => {
    expect(STEPS).toBe(9);
    expect(stepIndex(MIN_STEP)).toBe(0);
    expect(stepIndex(0.5)).toBe(4);
    expect(stepIndex(MAX_STEP)).toBe(8);
  });
});
