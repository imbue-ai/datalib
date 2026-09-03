// The relative-time formatter behind the "Last synced" column.
//
// Mostly boundary cases, because that is the only place this kind of
// function is ever wrong: one second either side of a unit change, the
// jump from 59 minutes to 1 hour, and the two inputs a real data root
// produces that a naive implementation mishandles — a stamp in a
// different UTC offset, and a stamp slightly in the future.

import { describe, expect, it } from "vitest";
import { compareStamps, formatRelative, formatStamp } from "./timeFormat";

/// A fixed "now" so the tests don't race the clock. Every case below
/// is expressed as an offset from it.
const NOW = Date.parse("2026-09-01T12:00:00+00:00");
const ago = (ms: number) => new Date(NOW - ms).toISOString();

const SEC = 1000;
const MIN = 60 * SEC;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;

describe("how long ago", () => {
  it("picks the coarsest unit that still has a whole number in it", () => {
    expect(formatRelative(ago(5 * MIN), NOW)).toBe("5 minutes ago");
    expect(formatRelative(ago(4 * HOUR), NOW)).toBe("4 hours ago");
    expect(formatRelative(ago(3 * DAY), NOW)).toBe("3 days ago");
    expect(formatRelative(ago(2 * WEEK), NOW)).toBe("2 weeks ago");
  });

  it("changes unit at the boundary, not before it", () => {
    expect(formatRelative(ago(59 * SEC), NOW)).toBe("seconds ago");
    expect(formatRelative(ago(60 * SEC), NOW)).toBe("1 minute ago");
    expect(formatRelative(ago(59 * MIN), NOW)).toBe("59 minutes ago");
    expect(formatRelative(ago(60 * MIN), NOW)).toBe("1 hour ago");
    expect(formatRelative(ago(23 * HOUR), NOW)).toBe("23 hours ago");
    expect(formatRelative(ago(24 * HOUR), NOW)).toBe("1 day ago");
    // 7 days is a week — the unit the user's "7 days ago" example sits
    // exactly on, so it is worth stating which side it lands.
    expect(formatRelative(ago(6 * DAY), NOW)).toBe("6 days ago");
    expect(formatRelative(ago(7 * DAY), NOW)).toBe("1 week ago");
  });

  it("gives the whole sub-minute band one still answer, not a countup", () => {
    // The point of the band is that nothing in it changes as you
    // watch, so every one of these has to read identically — a cell
    // that says "2 seconds ago" and then "3 seconds ago" a moment
    // later is the thing this replaced.
    expect(formatRelative(ago(0), NOW)).toBe("seconds ago");
    expect(formatRelative(ago(999), NOW)).toBe("seconds ago");
    expect(formatRelative(ago(1000), NOW)).toBe("seconds ago");
    expect(formatRelative(ago(2 * SEC), NOW)).toBe("seconds ago");
    expect(formatRelative(ago(30 * SEC), NOW)).toBe("seconds ago");
  });

  it("reads a future stamp forwards instead of as a negative past", () => {
    // Clock skew between the machine that ran the sync and the machine
    // reading this page is ordinary, and a moment of it must not read
    // as "-3 seconds ago".
    expect(formatRelative(ago(-500), NOW)).toBe("seconds ago");
    // Past a second it is a real disagreement rather than a moment of
    // skew, and the sub-minute band deliberately doesn't swallow it.
    expect(formatRelative(ago(-30 * SEC), NOW)).toBe("in 30 seconds");
    expect(formatRelative(ago(-3 * HOUR), NOW)).toBe("in 3 hours");
  });

  it("compares instants, not text, across UTC offsets", () => {
    // The tree stores each stamp with its source's own offset, so two
    // rows can carry different ones. These three strings are the same
    // instant written three ways; all must render identically.
    const sameInstant = [
      "2026-09-01T11:00:00+00:00",
      "2026-09-01T13:00:00+02:00",
      "2026-09-01T04:00:00-07:00",
    ];
    for (const iso of sameInstant) {
      expect(formatRelative(iso, NOW), iso).toBe("1 hour ago");
    }
  });

  it("has nothing to say about a row that never ran", () => {
    expect(formatRelative(null, NOW)).toBe("—");
    expect(formatStamp(null)).toBe("—");
  });

  it("shows an unparsable stamp verbatim rather than swallowing it", () => {
    // Better a string that looks wrong than a confident "seconds ago"
    // over a value nobody can read.
    expect(formatRelative("not a date", NOW)).toBe("not a date");
    expect(formatStamp("not a date")).toBe("not a date");
  });
});

describe("the exact stamp behind the hover", () => {
  it("renders the instant the relative form is about", () => {
    // Locale-dependent, so assert the parts that are not: it names the
    // right instant, on a 24-hour clock.
    const out = formatStamp("2026-09-01T17:03:27+02:00");
    expect(out).toContain("2026");
    expect(out).toMatch(/\b\d{2}:\d{2}:\d{2}\b/);
    // 24-hour: no am/pm marker, whatever the locale's separators.
    expect(out.toLowerCase()).not.toMatch(/\b[ap]\.?m\.?\b/);
  });
});

// Sorting the column. The rendered text is "5 minutes ago", and sorting
// on *that* would order the column alphabetically — "10 minutes ago"
// before "2 hours ago" before "seconds ago". AG Grid sorts the row value
// rather than what a renderer painted, so the text is never the key;
// what these pin is that the value comparison is on instants and not on
// the strings the value happens to be.
describe("ordering by when, not by how it reads", () => {
  const sorted = (xs: (string | null)[]) => [...xs].sort(compareStamps);

  it("orders by instant even when the text order disagrees", () => {
    // Same offset, so these DO sort correctly as text — included so a
    // regression that broke the ordinary case is caught too.
    const older = "2026-09-01T09:00:00+00:00";
    const newer = "2026-09-01T11:00:00+00:00";
    expect(sorted([newer, older])).toEqual([older, newer]);
  });

  it("compares across UTC offsets, where text order is a lie", () => {
    // 04:00-07:00 is 11:00Z — two hours AFTER 09:00Z — but sorts before
    // it as text on the leading hour digits. This is the case the tree's
    // "preserve the source's offset" convention makes real, and the one
    // a lexicographic sort gets backwards.
    const nineUtc = "2026-09-01T09:00:00+00:00";
    const elevenUtcWrittenLocal = "2026-09-01T04:00:00-07:00";
    expect(elevenUtcWrittenLocal < nineUtc, "premise: text order disagrees").toBe(true);
    expect(sorted([elevenUtcWrittenLocal, nineUtc])).toEqual([
      nineUtc,
      elevenUtcWrittenLocal,
    ]);
  });

  it("treats the same instant in two offsets as equal", () => {
    expect(
      compareStamps("2026-09-01T13:00:00+02:00", "2026-09-01T04:00:00-07:00"),
    ).toBe(0);
  });

  it("sorts 'never run' as forever ago, so reversing really reverses", () => {
    // A step that has never run is older than anything that has, not a
    // special case pinned to one end of the column. That keeps this a
    // plain total order: AG Grid negates a comparator's result for a
    // descending sort, and the whole column — nulls included — turns
    // over with it. It is also how one click on the header groups every
    // never-run step together.
    const a = "2026-09-01T09:00:00+00:00";
    const b = "2026-09-01T11:00:00+00:00";
    expect(sorted([a, null, b]), "ascending: oldest first").toEqual([null, a, b]);

    // Descending is exactly that reversed, which is the property worth
    // having. Modelled the way AG Grid applies it: negate the result.
    const desc = [a, null, b].sort((x, y) => -compareStamps(x, y));
    expect(desc, "descending is the ascending order reversed").toEqual([b, a, null]);
    expect(desc).toEqual([...sorted([a, null, b])].reverse());
  });

  it("treats two never-run rows as equal rather than ordering them", () => {
    // Neither is older than the other; whatever secondary order the
    // grid settles on is not this function's business.
    expect(compareStamps(null, null)).toBe(0);
  });
});
