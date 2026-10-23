// The date chip's rules: which stamps it reads, what it shows in the
// viewer's zone, its hover, its copy, and the searches it offers.

import { describe, expect, it } from "vitest";

import {
  dateCopyText,
  dateMenu,
  dateText,
  dateTitle,
  dayQuery,
  decorateDates,
  parseStamp,
  rewriteDatesForCopy,
  sideQuery,
  type Stamp,
} from "./dates";

const VIENNA = "Europe/Vienna";
const NOW = Date.parse("2026-04-18T10:05:00Z");

function stamp(datetime: string): Stamp {
  const s = parseStamp(datetime);
  if (!s) throw new Error(`no stamp in ${datetime}`);
  return s;
}

describe("what a datetime names", () => {
  it("reads an instant with its offset, and a day", () => {
    expect(parseStamp("2026-04-15T06:05:00-04:00")).toEqual({
      kind: "instant",
      ms: Date.parse("2026-04-15T10:05:00Z"),
      offsetMin: -240,
    });
    expect(parseStamp("2026-04-15T10:05:00.000Z")).toMatchObject({ offsetMin: 0 });
    expect(parseStamp("2026-04-15")).toEqual({ kind: "day", year: 2026, month: 4, day: 15 });
  });

  it("refuses what a render does not write", () => {
    expect(parseStamp("2026-04-15T10:05:00")).toBeNull();
    expect(parseStamp("2026-02-30")).toBeNull();
    expect(parseStamp("yesterday")).toBeNull();
  });
});

describe("what a date chip shows", () => {
  it("shows an instant in the viewer's zone and a day as written", () => {
    expect(dateText(stamp("2026-04-15T10:05:00+00:00"), VIENNA)).toBe("Wed, Apr 15, 2026, 12:05");
    expect(dateText(stamp("2026-04-15T10:05:00+00:00"), "UTC")).toBe("Wed, Apr 15, 2026, 10:05");
    // A day is the same day wherever it is read.
    expect(dateText(stamp("2026-04-15"), "Pacific/Kiritimati")).toBe("Wed, Apr 15, 2026");
  });

  it("hovers with the instant to the second, in UTC, as written, and how long ago", () => {
    expect(dateTitle(stamp("2026-04-15T06:05:30-04:00"), NOW, VIENNA).split("\n")).toEqual([
      expect.stringMatching(/^Wednesday, April 15, 2026.* 12:05:30 GMT\+2$/),
      "2026-04-15 10:05:30 UTC",
      "2026-04-15 06:05:30 -04:00 as written",
      "3 days ago",
    ]);
    // Written in UTC, or in the viewer's own zone: nothing more to say.
    expect(dateTitle(stamp("2026-04-15T12:05:30+02:00"), NOW, VIENNA).split("\n")).toHaveLength(3);
    expect(dateTitle(stamp("2026-04-15"), NOW, VIENNA)).toBe(
      "Wednesday, April 15, 2026\n2026-04-15 · 3 days ago",
    );
  });

  it("copies as shown, with its zone, so a paste names the same instant", () => {
    expect(dateCopyText(stamp("2026-04-15T10:05:00Z"), VIENNA)).toBe(
      "Wed, Apr 15, 2026, 12:05 GMT+2",
    );
    expect(dateCopyText(stamp("2026-04-15"), VIENNA)).toBe("Wed, Apr 15, 2026");
  });
});

describe("the searches a date offers", () => {
  it("finds its day in the viewer's zone", () => {
    // 23:30 UTC is already the next day in Vienna.
    expect(dayQuery(stamp("2026-04-15T23:30:00Z"), VIENNA)).toBe(
      "after:2026-04-16 before:2026-04-17",
    );
    expect(dayQuery(stamp("2026-04-30"), VIENNA)).toBe("after:2026-04-30 before:2026-05-01");
  });

  it("looks before or after the instant, or around a whole day", () => {
    const at = "2026-04-15T10:05:00+00:00";
    expect(sideQuery(at, stamp(at), "before")).toBe(`before:${at}`);
    expect(sideQuery("2026-04-15", stamp("2026-04-15"), "before")).toBe("before:2026-04-15");
    expect(sideQuery("2026-04-15", stamp("2026-04-15"), "after")).toBe("after:2026-04-16");
  });

  it("orders its menu as every chip's: copies, then searches", () => {
    const at = "2026-04-15T10:05:00Z";
    expect(dateMenu(at, stamp(at), VIENNA)).toEqual([
      { id: "copy-name", label: "Copy “Wed, Apr 15, 2026, 12:05 GMT+2”" },
      { id: "copy-id", label: `Copy ${at}` },
      { id: "search", label: "Everything on Wed, Apr 15, 2026", separator: true },
      { id: "before", label: "Everything before this" },
      { id: "after", label: "Everything after this" },
    ]);
  });
});

describe("in a document", () => {
  it("draws every time it can read as a chip and leaves the rest alone", () => {
    const root = document.createElement("div");
    root.innerHTML =
      '<time class="msg-ts" datetime="2026-04-15T10:05:00+00:00" title="t">Wed Apr 15th</time>' +
      '<time datetime="soon">soon</time>';
    decorateDates(root, NOW);
    const [chip, odd] = Array.from(root.querySelectorAll("time"));
    expect(chip.className).toBe("msg-ts chip handle-chip date-chip");
    expect(chip.draggable).toBe(true);
    expect(chip.textContent).toBe(dateText(stamp("2026-04-15T10:05:00Z")));
    expect(chip.title).toContain("2026-04-15 10:05:00 UTC");
    expect(odd.outerHTML).toBe('<time datetime="soon">soon</time>');
  });

  it("copies a date chip as a plain time with its zone in the text", () => {
    const root = document.createElement("div");
    root.innerHTML = '<p>At <time datetime="2026-04-15">x</time>.</p>';
    decorateDates(root, NOW);
    expect(rewriteDatesForCopy(root)).toBe(true);
    expect(root.innerHTML).toBe('<p>At <time datetime="2026-04-15">Wed, Apr 15, 2026</time>.</p>');
  });
});
