import { describe, expect, it } from "vitest";

import { UTC, type Zone } from "@/config/timeView";
import {
  PRESETS,
  alignedWindowSpan,
  parseWindow,
  storedText,
  windowLabel,
  windowSpan,
  windowText,
} from "./timeWindow";

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const BERLIN: Zone = { kind: "named", name: "Europe/Berlin" };

describe("a typed window", () => {
  it("reads durations in every spelling, m as minutes and mo as months", () => {
    expect(parseWindow("15m", UTC)).toEqual({ kind: "last", ms: 15 * MIN });
    expect(parseWindow("1h", UTC)).toEqual({ kind: "last", ms: HOUR });
    expect(parseWindow("Last 6 hours", UTC)).toEqual({ kind: "last", ms: 6 * HOUR });
    expect(parseWindow("1.5h", UTC)).toEqual({ kind: "last", ms: 90 * MIN });
    expect(parseWindow("2w", UTC)).toEqual({ kind: "last", ms: 14 * DAY });
    expect(parseWindow("1mo", UTC)).toEqual({ kind: "last", ms: 30 * DAY });
    expect(parseWindow("ALL", UTC)).toEqual({ kind: "all" });
  });

  it("refuses what is none of the three", () => {
    for (const bad of ["", "soon", "0h", "5 parsecs", "2369-04-14", "a to b"]) {
      expect(parseWindow(bad, UTC), bad).toBeNull();
    }
  });

  it("reads two dates in the clock's zone, and a bare date as its whole day", () => {
    const from = Date.parse("2369-04-14T05:00:00Z");
    expect(parseWindow("2369-04-14 07:00 to 2369-04-14 09:00", BERLIN)).toEqual({
      kind: "between",
      fromMs: from,
      toMs: from + 2 * HOUR,
    });
    expect(parseWindow("2369-04-14 .. 2369-04-15", UTC)).toEqual({
      kind: "between",
      fromMs: Date.parse("2369-04-14T00:00:00Z"),
      toMs: Date.parse("2369-04-16T00:00:00Z"),
    });
  });

  it("refuses a range that ends before it starts", () => {
    expect(parseWindow("2369-04-14 09:00 to 2369-04-14 07:00", UTC)).toBeNull();
  });
});

describe("writing a window back", () => {
  /** `90d` must not come back as `3mo`, nor `7d` as `1w`. */
  it("reads back as itself, in the units it was typed in", () => {
    for (const p of PRESETS) expect(windowText(parseWindow(p, UTC)!, UTC)).toBe(p);
    const range = "2369-04-14 07:00 to 2369-04-14 09:00";
    expect(windowText(parseWindow(range, BERLIN)!, BERLIN)).toBe(range);
  });

  /** A range saved with the clock on Berlin must still be the same two
   *  instants when the card is opened with the clock on another zone. */
  it("stores a range in UTC, so another zone reads the same instants", () => {
    const w = parseWindow("2369-04-14 07:00 to 2369-04-14 09:00", BERLIN)!;
    expect(storedText(w)).toBe("2369-04-14T05:00:00.000Z to 2369-04-14T07:00:00.000Z");
    expect(parseWindow(storedText(w), { kind: "named", name: "Asia/Tokyo" })).toEqual(w);
  });

  /** A layout saved before the window box held `range=30d`. */
  it("reads every preset the old range buttons stored", () => {
    for (const old of ["30d", "90d", "1y", "all"]) expect(parseWindow(old, UTC)).not.toBeNull();
  });

  it("labels presets in words", () => {
    expect(windowLabel(parseWindow("1h", UTC)!)).toBe("Last 1 hour");
    expect(windowLabel(parseWindow("90d", UTC)!)).toBe("Last 90 days");
    expect(windowLabel(parseWindow("all", UTC)!)).toBe("All");
  });
});

describe("what a window spans", () => {
  const newest = 100 * DAY;

  it("ends a duration at the newest point, and takes two dates as they are", () => {
    expect(windowSpan({ kind: "last", ms: HOUR }, newest)).toEqual({
      fromMs: newest - HOUR,
      toMs: newest,
    });
    expect(windowSpan({ kind: "all" }, newest)).toBeNull();
    expect(windowSpan({ kind: "between", fromMs: 1, toMs: 2 }, newest)).toEqual({
      fromMs: 1,
      toMs: 2,
    });
  });

  it("with starts aligned, is the first that much of each line", () => {
    const longest = 42 * MIN;
    expect(alignedWindowSpan({ kind: "last", ms: 15 * MIN }, longest)).toEqual({
      fromMs: 0,
      toMs: 15 * MIN,
    });
    expect(alignedWindowSpan({ kind: "last", ms: DAY }, longest)).toEqual({
      fromMs: 0,
      toMs: longest,
    });
    expect(alignedWindowSpan({ kind: "between", fromMs: 1, toMs: 2 }, longest)).toEqual({
      fromMs: 0,
      toMs: longest,
    });
  });
});
