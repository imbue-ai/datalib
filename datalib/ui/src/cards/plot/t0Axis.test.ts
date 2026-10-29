// A time axis drawn from T0: where its ticks fall and what they say.

import { describe, expect, it } from "vitest";

import { t0TickTexts, t0Ticks } from "./t0Axis";

const T0 = Date.parse("2026-10-26T09:21:57.190Z");
const S = 1000;
const H = 3_600_000;
const D = 24 * H;
const Y = 365.2425 * D;

/** The labels of the ticks `t0Ticks` places over `[from, to]` (ms from
 *  T0) at most `count` intervals apart. */
function axis(from: number, to: number, count = 4): string[] {
  const ticks = t0Ticks(T0 + from, T0 + to, T0, count);
  return t0TickTexts(ticks, T0, to - from);
}

describe("a time axis from T0", () => {
  it("puts ticks on whole steps from T0, wherever the span starts", () => {
    const ticks = t0Ticks(T0 - 7 * S, T0 + 13 * S, T0, 4);
    expect(ticks.map((t) => (t - T0) / S)).toEqual([-5, 0, 5, 10]);
  });

  it("labels each tick in the units its spacing needs, zero units left out", () => {
    expect(axis(-7 * S, 13 * S)).toEqual(["-5s", "T0", "+5s", "+10s"]);
    expect(axis(0, 2 * D)).toEqual(["T0", "+12h", "+1d", "+1d 12h", "+2d"]);
    expect(axis(-90 * 60 * S, 90 * 60 * S, 6)).toEqual([
      "-1h 30m",
      "-1h",
      "-30m",
      "T0",
      "+30m",
      "+1h",
      "+1h 30m",
    ]);
    expect(axis(0, 800)).toEqual(["T0", "+200ms", "+400ms", "+600ms", "+800ms"]);
  });

  /** A span of centuries read as ten-digit day counts. */
  it("counts calendar years past a year, then days", () => {
    // 2026-10-26 plus a calendar year is 365 days on.
    expect(axis(350 * D, 400 * D, 2)).toEqual(["+360d", "+1y 25d"]);
    expect(axis(-2 * Y - D, 1.1 * Y)).toEqual(["-2y", "-1y", "T0", "+1y"]);
    expect(axis(0, 10.5 * Y)).toEqual(["T0", "+5y", "+10y"]);
  });
});
