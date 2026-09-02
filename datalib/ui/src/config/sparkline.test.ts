import { describe, expect, it } from "vitest";
import { calibrationMax, sparkline, type UsageSample } from "./sparkline";

/// A fixed window, so every expectation below is in round numbers:
/// 100 px wide, 10 px tall, five minutes ending at t=0 by these stamps.
const NOW = Date.parse("2026-09-02T10:05:00-07:00");
const WINDOW = 5 * 60 * 1000;
const BOX = { nowMs: NOW, windowMs: WINDOW, width: 100, height: 10, inset: 0 };

function at(minutesAgo: number, bytes: number): UsageSample {
  return { at: new Date(NOW - minutesAgo * 60_000).toISOString(), bytes };
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

  it("shares one scale across rows, so height means size", () => {
    const big = sparkline([at(1, 1000)], { ...BOX, max: 1000 })!;
    const small = sparkline([at(1, 10)], { ...BOX, max: 1000 })!;
    expect(big.line).toBe("0,0 100,0");
    // 1% of the box: a sliver, not a line at the same height as the
    // 1000-byte row. That difference is the whole point of calibrating.
    expect(small.line).toBe("0,9.9 100,9.9");
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
    expect(sparkline([{ at: "whenever", bytes: 5 }], { ...BOX, max: 100 })).toBeNull();
  });

  it("survives a zero maximum rather than dividing by it", () => {
    const s = sparkline([at(1, 0)], { ...BOX, max: 0 })!;
    expect(s.line).toBe("0,10 100,10");
  });
});

describe("calibrationMax", () => {
  it("covers history as well as the present", () => {
    // The row shrank: its own past is the tallest thing it has to
    // draw, and a max taken from `bytes` alone would put it off the
    // top of the box.
    expect(
      calibrationMax([{ bytes: 10, history: [at(4, 900), at(1, 10)] }]),
    ).toBe(900);
  });

  it("ignores a row with nothing on disk", () => {
    expect(calibrationMax([{ bytes: null, history: [] }, { bytes: 7, history: [] }])).toBe(7);
  });
});
