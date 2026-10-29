// The display-time setting's rules: reading an instant in a zone, the
// day bounds a search names, finding a zone, and reading a stored choice.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_VIEW,
  LOCAL,
  UTC,
  WRITTEN,
  dayBounds,
  formatAt,
  millisText,
  parseInstant,
  readStoredRecent,
  readStoredView,
  searchZones,
  sinceT0,
  wallClock,
  withRecent,
  zoneName,
  type TimeView,
} from "./timeView";

const named = (name: string) => ({ kind: "named" as const, name });
const NOW = Date.parse("2026-04-15T12:00:00Z");

describe("an instant and the offset it was written in", () => {
  it("reads the offset of a stamp that has one", () => {
    expect(parseInstant("2026-04-15T06:05:00-04:00")).toEqual({
      ms: Date.parse("2026-04-15T10:05:00Z"),
      offsetMin: -240,
    });
    expect(parseInstant("2026-04-15T10:05:00.123Z")).toMatchObject({ offsetMin: 0 });
    expect(parseInstant("2026-04-15T10:05:00+0530")).toMatchObject({ offsetMin: 330 });
  });

  it("has no offset for a stamp that names none, and no instant for text", () => {
    expect(parseInstant("2026-04-15")).toMatchObject({ offsetMin: null });
    expect(parseInstant("never")).toBeNull();
  });
});

describe("reading an instant in a zone", () => {
  const at = { ms: Date.parse("2026-04-15T23:30:00Z"), offsetMin: -240 };

  it("reads a named zone's wall clock, and the stamp's own as written", () => {
    expect(wallClock(at, named("Europe/Vienna"))).toMatchObject({ day: 16, hour: 1, minute: 30 });
    expect(wallClock(at, WRITTEN)).toMatchObject({ day: 15, hour: 19, minute: 30 });
    expect(zoneName(at, WRITTEN)).toBe("GMT-4");
  });

  /** "As written" on a stamp with no offset of its own (a run's start,
   *  an axis tick) has nothing to read but this computer's clock. */
  it("reads a stamp with no offset as this computer does, as written", () => {
    const bare = { ms: at.ms, offsetMin: null };
    expect(wallClock(bare, WRITTEN)).toEqual(wallClock(bare, LOCAL));
  });

  it("writes the day and clock in the view's style", () => {
    const view: TimeView = { zone: UTC, days: "iso", hours: "h12" };
    expect(formatAt(at, view, { day: "short", clock: true, seconds: true })).toBe(
      "2026-04-15 11:30:00 PM",
    );
    expect(formatAt(at, { ...view, hours: "h23" }, { clock: true, zoneName: true })).toBe(
      "23:30 UTC",
    );
    expect(formatAt(at, { ...DEFAULT_VIEW, zone: UTC }, { day: "medium" })).toBe("Apr 15, 2026");
  });
});

describe("the day a search names", () => {
  it("is the zone's two midnights, each with its own offset", () => {
    const at = { ms: Date.parse("2026-11-01T12:00:00Z"), offsetMin: null };
    // Los Angeles leaves daylight time that morning.
    expect(dayBounds(at, named("America/Los_Angeles"))).toEqual([
      "2026-11-01T00:00:00-07:00",
      "2026-11-02T00:00:00-08:00",
    ]);
    expect(dayBounds(at, UTC)).toEqual(["2026-11-01T00:00:00+00:00", "2026-11-02T00:00:00+00:00"]);
  });
});

describe("finding a zone", () => {
  const zones = ["America/Los_Angeles", "Asia/Kolkata", "Europe/Vienna", "UTC"];

  it("finds a zone by its place or by its offset now", () => {
    expect(searchZones("los ang", zones, NOW).map((z) => z.name)).toEqual(["America/Los_Angeles"]);
    expect(searchZones("+05:30", zones, NOW)).toEqual([{ name: "Asia/Kolkata", offset: "+05:30" }]);
    expect(searchZones("utc-7", zones, NOW).map((z) => z.name)).toEqual(["America/Los_Angeles"]);
    expect(searchZones("  ", zones, NOW)).toEqual([]);
  });

  it("keeps the last few named zones, the newest first", () => {
    expect(withRecent(["Europe/Vienna", "Asia/Kolkata"], named("Asia/Kolkata"))).toEqual([
      "Asia/Kolkata",
      "Europe/Vienna",
    ]);
    expect(withRecent(["A", "B", "C", "D", "E"], named("F"))).toEqual(["F", "A", "B", "C", "D"]);
    // UTC, local and as-written have entries of their own.
    expect(withRecent(["Europe/Vienna"], UTC)).toEqual(["Europe/Vienna"]);
    expect(withRecent(["Europe/Vienna"], WRITTEN)).toEqual(["Europe/Vienna"]);
  });
});

describe("a stored choice", () => {
  it("reads back what was saved", () => {
    const view: TimeView = { zone: named("Europe/Vienna"), days: "iso", hours: "h12" };
    expect(readStoredView(JSON.stringify(view))).toEqual(view);
    expect(readStoredView(JSON.stringify({ ...view, zone: WRITTEN }))).toMatchObject({
      zone: WRITTEN,
    });
  });

  /** A zone this engine does not know, or a value from a newer build,
   *  must not leave every date in the app unreadable. */
  it("reads anything it cannot use as the default", () => {
    expect(readStoredView(null)).toEqual(DEFAULT_VIEW);
    expect(readStoredView("{not json")).toEqual(DEFAULT_VIEW);
    expect(readStoredView(JSON.stringify({ zone: named("Mars/Olympus"), days: "x" }))).toEqual(
      DEFAULT_VIEW,
    );
    expect(readStoredRecent(JSON.stringify(["Europe/Vienna", "Mars/Olympus", 3]))).toEqual([
      "Europe/Vienna",
    ]);
  });
});

describe("a log's stamp", () => {
  it("is the date and the time of day to the millisecond, one clock for every stamp", () => {
    const at = (iso: string) => parseInstant(iso)!;
    expect(millisText(at("2026-09-11T15:10:41.113Z"), named("Europe/Vienna"))).toBe(
      "2026-09-11 17:10:41.113",
    );
    expect(millisText(at("2026-09-11T17:10:41.113+02:00"), UTC)).toBe("2026-09-11 15:10:41.113");
    expect(millisText(at("2026-09-11T17:10:41.000000+02:00"), WRITTEN)).toBe(
      "2026-09-11 17:10:41.000",
    );
  });
});

describe("a date from T0", () => {
  const T0 = Date.parse("2026-10-26T09:21:57.190Z");
  const after = (ms: number) => sinceT0(T0 + ms, T0);

  it("leads with the largest unit that is not zero, unpadded", () => {
    expect(after(0)).toBe("0.000");
    expect(after(1250)).toBe("+1.250");
    expect(after(12_345)).toBe("+12.345");
    expect(after(65_200)).toBe("+1:05.200");
    expect(after(3_600_000)).toBe("+1:00:00.000");
    expect(after(36_000_000 + 4_000)).toBe("+10:00:04.000");
    expect(after(3 * 86_400_000 + 4 * 3_600_000 + 5 * 60_000 + 6_789)).toBe("+3d 04:05:06.789");
  });

  it("is negative before T0", () => {
    expect(after(-500)).toBe("-0.500");
    expect(after(-245_000)).toBe("-4:05.000");
    expect(after(-86_400_000)).toBe("-1d 00:00:00.000");
  });
});
