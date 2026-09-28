import { describe, expect, it } from "vitest";
import { ownRange, sparkline, windowDelta, type Sample } from "./sparkline";

/// A fixed window, so every expectation below is in round numbers:
/// 100 px wide, 10 px tall, five minutes ending at t=0 by these stamps.
const NOW = Date.parse("2026-09-02T10:05:00-07:00");
const WINDOW = 5 * 60 * 1000;
const BOX = { nowMs: NOW, windowMs: WINDOW, width: 100, height: 10, inset: 0, min: 0 };

function at(minutesAgo: number, value: number): Sample {
  return { at: new Date(NOW - minutesAgo * 60_000).toISOString(), value };
}

describe("sparkline", () => {
  it("draws a step, not a slope, between two samples", () => {
    // One change, halfway through the window: 0 → 100 against a max of
    // 100, so the line runs along the floor and then along the ceiling.
    const s = sparkline([at(5, 0), at(2.5, 100)], { ...BOX, max: 100 })!;
    expect(s.line).toBe("0,10 50,10 50,0 100,0");
  });

  it("carries the last value out to the right edge", () => {
    // Nothing has moved since two minutes ago; the line must reach the
    // present rather than stopping where the samples do.
    const s = sparkline([at(2, 50)], { ...BOX, max: 100 })!;
    expect(s.line.endsWith("100,5")).toBe(true);
  });

  it("opens the window at the last sample from before it", () => {
    // The only sample is an hour old — the value it recorded is the
    // value for the whole window, and the line is flat at it. This is
    // the case that made the carry-in sample worth serving: without it
    // the series would draw as nothing.
    const s = sparkline([at(60, 80)], { ...BOX, max: 100 })!;
    expect(s.line).toBe("0,2 100,2");
  });

  it("clamps a sample from the future to the right edge", () => {
    const s = sparkline([at(5, 0), at(-10, 100)], { ...BOX, max: 100 })!;
    // The step lands at x=100 rather than off the end of the box.
    expect(s.line).toBe("0,10 100,10 100,0");
  });

  it("can plot against a floor, for a series that moves by fractions", () => {
    // 40.0 GB → 40.4 GB. Against zero this is a flat line at the top;
    // against its own range it is the change you wanted to see.
    const s = sparkline([at(4, 40_000_000_000), at(2, 40_400_000_000)], {
      ...BOX,
      min: 40_000_000_000,
      max: 40_400_000_000,
    })!;
    expect(s.line).toBe("0,10 60,10 60,0 100,0");
  });

  it("fills from the floor at both ends", () => {
    const s = sparkline([at(1, 100)], { ...BOX, max: 100 })!;
    expect(s.area).toBe("0,10 0,0 100,0 100,10");
  });

  it("returns nothing when there is nothing to draw", () => {
    expect(sparkline([], { ...BOX, max: 100 })).toBeNull();
    // A stamp we can't read is dropped rather than guessed at.
    expect(sparkline([{ at: "whenever", value: 5 }], { ...BOX, max: 100 })).toBeNull();
  });

  it("survives a zero maximum rather than dividing by it", () => {
    const s = sparkline([at(1, 0)], { ...BOX, max: 0 })!;
    expect(s.line).toBe("0,10 100,10");
  });
});

describe("ownRange", () => {
  it("is the series' own span, not zero to its size", () => {
    // A 10 MB source that grew by 1 MB fills the box, however small it
    // is beside a 40 GB neighbour. That is the jump worth seeing.
    expect(ownRange(11, [at(4, 10), at(1, 11)], NOW, WINDOW)).toEqual({ min: 10, max: 11 });
  });

  it("covers the value the window opens at and the present", () => {
    // The hour-old sample is what the line starts from; the present
    // has moved past the last recorded sample.
    expect(ownRange(30, [at(60, 50), at(2, 40)], NOW, WINDOW)).toEqual({ min: 30, max: 50 });
  });

  it("ignores history the window has slid past", () => {
    expect(ownRange(5, [at(90, 1000), at(60, 5)], NOW, WINDOW)).toEqual({ min: 4.95, max: 5.05 });
  });

  it("straddles a series that has not moved", () => {
    expect(ownRange(0, [at(1, 0)], NOW, WINDOW)).toEqual({ min: 0, max: 1 });
  });
});

describe("windowDelta", () => {
  it("is the present against the value the window opened at", () => {
    expect(windowDelta(70, [at(60, 50), at(2, 60)], NOW, WINDOW)).toBe(20);
  });

  it("is zero once every step has slid out of the window", () => {
    expect(windowDelta(60, [at(60, 50), at(10, 60)], NOW, WINDOW)).toBe(0);
  });

  it("is null with nothing to compare against", () => {
    expect(windowDelta(5, [], NOW, WINDOW)).toBeNull();
  });
});
