// The date chip's rules: which stamps it reads, what it shows in the
// view's zone and format, its hover, its copy, and the searches it offers.

import { describe, expect, it } from "vitest";

import {
  dateCopyText,
  dateMenu,
  dateText,
  dateTitle,
  datesInText,
  dayQuery,
  dateCell,
  fittingDateCell,
  millisDateCell,
  shortDateTexts,
  decorateDates,
  parseStamp,
  rangeEndText,
  rewriteDatesForCopy,
  sideQuery,
  type Stamp,
} from "./dates";
import { DEFAULT_VIEW, LOCAL, WRITTEN, type TimeView } from "@/config/timeView";

/** The default view, reading in the named zone. */
function inZone(name: string): TimeView {
  return { ...DEFAULT_VIEW, zone: { kind: "named", name } };
}

const VIENNA = inZone("Europe/Vienna");
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
  it("shows an instant in the view's zone and a day as written", () => {
    expect(dateText(stamp("2026-04-15T10:05:00+00:00"), VIENNA)).toBe("Wed, Apr 15, 2026, 12:05");
    expect(dateText(stamp("2026-04-15T10:05:00+00:00"), inZone("UTC"))).toBe(
      "Wed, Apr 15, 2026, 10:05",
    );
    // A day is the same day wherever it is read.
    expect(dateText(stamp("2026-04-15"), inZone("Pacific/Kiritimati"))).toBe("Wed, Apr 15, 2026");
  });

  it("hovers with the instant to the second, in UTC, as written, and how long ago", () => {
    expect(dateTitle(stamp("2026-04-15T06:05:30-04:00"), NOW, VIENNA).split("\n")).toEqual([
      expect.stringMatching(/^Wednesday, April 15, 2026.* 12:05:30 GMT\+2$/),
      "2026-04-15 10:05:30 UTC",
      "2026-04-15 06:05:30 -04:00 as written",
      "3 days ago",
    ]);
    // Written in UTC, or in the view's own zone: nothing more to say.
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

describe("a date in a narrow column", () => {
  /// A date column cut the time off: "Mon, Apr 14, 2369, 12…".
  it("falls back to the day and clock as numbers, then the day", () => {
    expect(shortDateTexts(stamp("2369-04-14T10:05:00+00:00"), VIENNA)).toEqual([
      "2369-04-14 12:05",
      "2369-04-14",
    ]);
    expect(shortDateTexts(stamp("2369-04-14"), VIENNA)).toEqual(["2369-04-14", "2369-04-14"]);
  });

  it("holds the shorter forms beside the whole date, for its width to pick from", () => {
    const cell = fittingDateCell("2369-04-14T10:05:00+00:00");
    expect(cell.className).toBe("date-fit");
    const chip = cell.querySelector<HTMLElement>("time.date-chip")!;
    expect(chip.getAttribute("datetime")).toBe("2369-04-14T10:05:00+00:00");
    expect(chip.textContent).toBe(chip.getAttribute("aria-label"));
    expect(chip.dataset.day).toBe(shortDateTexts(stamp("2369-04-14T10:05:00+00:00"))[1]);
  });
});

describe("dates in a snippet", () => {
  /// A calendar result's snippet read "Fri Sep 18th, 2026 at 19:00", in
  /// the event's zone, beside chips that said "Sat, Sep 19, 2026, 04:00".
  it("shows each instant as a chip says it, and leaves other digits alone", () => {
    expect(
      datesInText(
        "Dinner 2026-09-18T19:00:00-07:00 – 2026-09-18T22:00:00-07:00 on deck 10",
        VIENNA,
      ),
    ).toBe("Dinner Sat, Sep 19, 2026, 04:00 – Sat, Sep 19, 2026, 07:00 on deck 10");
    expect(datesInText("stardate 2369-04-15, 47988.1", VIENNA)).toBe(
      "stardate 2369-04-15, 47988.1",
    );
  });
});

describe("the view a date is read in", () => {
  it("reads each stamp in the offset it was written in", () => {
    const view = { ...DEFAULT_VIEW, zone: WRITTEN };
    expect(dateText(stamp("2026-04-15T06:05:00-04:00"), view)).toBe("Wed, Apr 15, 2026, 06:05");
    expect(dateText(stamp("2026-04-15T18:35:00+05:30"), view)).toBe("Wed, Apr 15, 2026, 18:35");
    expect(dateCopyText(stamp("2026-04-15T18:35:00+05:30"), view)).toBe(
      "Wed, Apr 15, 2026, 18:35 GMT+5:30",
    );
  });

  it("writes ISO days and a 12-hour clock when asked", () => {
    const at = stamp("2026-04-15T15:05:00Z");
    expect(dateText(at, { ...inZone("UTC"), days: "iso" })).toBe("2026-04-15 15:05");
    expect(dateText(at, { ...inZone("UTC"), days: "iso", hours: "h12" })).toBe(
      "2026-04-15 3:05 PM",
    );
    expect(dateText(at, { ...inZone("UTC"), hours: "h12" })).toMatch(
      /^Wed, Apr 15, 2026, 3:05\sPM$/,
    );
    expect(dateText(stamp("2026-04-15"), { ...VIENNA, days: "iso" })).toBe("2026-04-15");
  });
});

describe("the searches a date offers", () => {
  it("finds its day in the view's zone, with that zone's midnights written out", () => {
    // 23:30 UTC is already the next day in Vienna. The search would read
    // a bare day as midnight wherever it runs, so the bounds say whose.
    expect(dayQuery(stamp("2026-04-15T23:30:00Z"), VIENNA)).toBe(
      "after:2026-04-16T00:00:00+02:00 before:2026-04-17T00:00:00+02:00",
    );
    // A day is the same day everywhere.
    expect(dayQuery(stamp("2026-04-30"), VIENNA)).toBe("after:2026-04-30 before:2026-05-01");
  });

  /** A day that starts in winter time and ends in summer time: one
   *  offset for both midnights would miss or add an hour. */
  it("gives each midnight its own offset across a DST change", () => {
    expect(dayQuery(stamp("2026-03-08T15:00:00Z"), inZone("America/New_York"))).toBe(
      "after:2026-03-08T00:00:00-05:00 before:2026-03-09T00:00:00-04:00",
    );
  });

  it("finds the day the stamp was written on, read as written", () => {
    const view = { ...DEFAULT_VIEW, zone: WRITTEN };
    expect(dayQuery(stamp("2026-04-15T23:30:00-04:00"), view)).toBe(
      "after:2026-04-15T00:00:00-04:00 before:2026-04-16T00:00:00-04:00",
    );
  });

  it("writes a bare day when the view reads as this machine does", () => {
    // Noon UTC is the same day in every zone within eleven hours of it.
    expect(dayQuery(stamp("2026-04-15T12:00:00Z"), { ...DEFAULT_VIEW, zone: LOCAL })).toBe(
      "after:2026-04-15 before:2026-04-16",
    );
  });

  it("looks before or after the instant, or around a whole day", () => {
    const at = "2026-04-15T10:05:00+00:00";
    expect(sideQuery(at, stamp(at), "before")).toBe(`before:${at}`);
    expect(sideQuery("2026-04-15", stamp("2026-04-15"), "before")).toBe("before:2026-04-15");
    expect(sideQuery("2026-04-15", stamp("2026-04-15"), "after")).toBe("after:2026-04-16");
  });

  it("orders its menu as every chip's: copies, then searches, then T0", () => {
    const at = "2026-04-15T10:05:00Z";
    expect(dateMenu(at, stamp(at), VIENNA)).toEqual([
      { id: "copy-name", label: "Copy “Wed, Apr 15, 2026, 12:05 GMT+2”" },
      { id: "copy-id", label: `Copy ${at}` },
      { id: "search", label: "Everything on Wed, Apr 15, 2026", separator: true },
      { id: "before", label: "Everything before this" },
      { id: "after", label: "Everything after this" },
      { id: "use-t0", label: "Use as T0", separator: true },
    ]);
    // A day names no instant to measure from.
    expect(dateMenu("2026-04-15", stamp("2026-04-15"), VIENNA).map((e) => e.id)).not.toContain(
      "use-t0",
    );
  });
});

describe("a span of time", () => {
  /** A calendar event's "When" was its own text, in its own format, in
   *  the event's zone, beside every other date on the page drawn as a
   *  chip in the reader's. */
  it("ends on the same day as its clock alone, in the reader's zone", () => {
    const start = stamp("2026-09-18T19:00:00-07:00");
    expect(
      rangeEndText(start, stamp("2026-09-18T22:00:00-07:00"), inZone("America/Los_Angeles")),
    ).toBe("22:00");
    // 14:00–16:00 in Los Angeles is 23:00–01:00 in Vienna: there the end
    // says its own day.
    const afternoon = stamp("2026-09-18T14:00:00-07:00");
    expect(rangeEndText(afternoon, stamp("2026-09-18T16:00:00-07:00"), VIENNA)).toBeNull();
    expect(rangeEndText(stamp("2026-09-18"), stamp("2026-09-19"), VIENNA)).toBeNull();
  });

  it("draws the end of a span by that rule", () => {
    const root = document.createElement("div");
    root.innerHTML =
      '<span class="time-range"><time datetime="2026-09-18T17:00:00+00:00">a</time> – ' +
      '<time datetime="2026-09-18T19:30:00+00:00">b</time></span>';
    decorateDates(root, NOW, inZone("UTC"));
    const [from, to] = Array.from(root.querySelectorAll("time"));
    expect(from!.textContent).toBe("Fri, Sep 18, 2026, 17:00");
    expect(to!.textContent).toBe("19:30");
    expect(to!.title).toContain("2026-09-18 19:30:00 UTC");
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

describe("from T0", () => {
  const T0 = "2026-04-15T10:05:00.250Z";
  const FROM_T0: TimeView = { ...VIENNA, t0: { ms: Date.parse(T0), offsetMin: 0 } };

  it("shows each instant as its distance from T0, and a day as a day", () => {
    expect(dateText(stamp("2026-04-15T10:06:05.450Z"), FROM_T0)).toBe("+1:05.200");
    expect(dateText(stamp("2026-04-15T12:05:00.250+02:00"), FROM_T0)).toBe("0.000");
    expect(dateText(stamp("2026-04-14T10:05:00.250Z"), FROM_T0)).toBe("-1d 00:00:00.000");
    expect(dateText(stamp("2026-04-15"), FROM_T0)).toBe("Wed, Apr 15, 2026");
    expect(shortDateTexts(stamp("2026-04-15T10:05:01.250Z"), FROM_T0)).toEqual([
      "+1.000",
      "+1.000",
    ]);
  });

  /** A distance from T0 means nothing pasted outside this page. */
  it("still copies and hovers as the date", () => {
    const at = stamp("2026-04-15T10:06:05Z");
    expect(dateCopyText(at, FROM_T0)).toBe("Wed, Apr 15, 2026, 12:06 GMT+2");
    expect(dateTitle(at, NOW, FROM_T0)).toContain("2026-04-15 10:06:05 UTC");
  });

  it("draws each end of a span from T0", () => {
    const start = stamp("2026-04-15T10:05:00Z");
    expect(rangeEndText(start, stamp("2026-04-15T11:00:00Z"), FROM_T0)).toBeNull();
  });

  it("offers to clear T0, and not to set it again on T0 itself", () => {
    const ids = (at: string) => dateMenu(at, stamp(at), FROM_T0).map((e) => e.id);
    expect(ids("2026-04-15T10:06:05Z").slice(5)).toEqual(["use-t0", "clear-t0"]);
    expect(ids(T0).slice(5)).toEqual(["clear-t0"]);
    expect(ids("2026-04-15").slice(5)).toEqual(["clear-t0"]);
  });
});

describe("a log's date", () => {
  /** The log's Time column was plain text, with none of a chip's hover,
   *  copy or menu. */
  it("is a chip to the millisecond, or from T0", () => {
    const at = "2026-09-22T12:07:47.190000+00:00";
    const chip = millisDateCell(at, NOW, VIENNA);
    expect(chip.matches("time.chip.date-chip[datetime]")).toBe(true);
    expect(chip.textContent).toBe("2026-09-22 14:07:47.190");
    expect(chip.title).toContain("2026-09-22 12:07:47 UTC");

    const t0 = { ms: Date.parse("2026-09-22T12:07:40Z"), offsetMin: 0 };
    const fromT0 = millisDateCell(at, NOW, { ...VIENNA, t0 });
    expect(fromT0.textContent).toBe("+7.190");
    expect(fromT0.classList.contains("date-from-t0")).toBe(true);
  });

  /** Cut from the right, a narrow Time column kept the date every line
   *  shares and lost the seconds that tell them apart. */
  it("gives way on the left in a log or from T0, and on the right elsewhere", () => {
    const at = "2026-09-22T12:07:47.190000+00:00";
    const leftCut = (el: HTMLElement) => !!el.querySelector(".chip-text.chip-elide-left");
    expect(leftCut(millisDateCell(at, NOW, VIENNA))).toBe(true);
    expect(leftCut(dateCell(at, NOW, VIENNA))).toBe(false);
    const t0 = { ms: Date.parse("2026-09-22T12:07:40Z"), offsetMin: 0 };
    expect(leftCut(dateCell(at, NOW, { ...VIENNA, t0 }))).toBe(true);
    expect(millisDateCell(at, NOW, VIENNA).querySelector(".chip-ltr")?.textContent).toBe(
      "2026-09-22 14:07:47.190",
    );
  });
});
