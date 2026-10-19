import { describe, expect, it } from "vitest";

import {
  bandOf,
  axisTicks,
  decodeState,
  displayUnit,
  encodeState,
  extent,
  formatTick,
  formatTime,
  formatTimeTick,
  grouped,
  lineLabels,
  lineOf,
  nearest,
  newestMs,
  niceTicks,
  rangeBounds,
  refusal,
  toMs,
  unitText,
  type Answered,
  type Listed,
} from "./timeSeries";

function listed(over: Partial<Listed>): Listed {
  return {
    time_series_id: "id",
    source_id: "riker",
    about_uuid: null,
    about_name: null,
    name: "resting_hr",
    human_name: "Resting HR",
    unit: "{beats}/min",
    unit_label: "beats/min",
    quantity: "T-1{beats}",
    unit_factor: 1 / 60,
    unit_offset: 0,
    value_type: "float",
    time_kind: "day",
    point_count: 1,
    first_t: 0,
    last_t: 0,
    ...over,
  };
}

const celsius = {
  unit: "Cel",
  unit_label: "°C",
  quantity: "Θ1",
  unit_factor: 1,
  unit_offset: 273.15,
};
const minutes = { unit: "min", unit_label: "min", quantity: "T1", unit_factor: 60, unit_offset: 0 };

describe("the axes", () => {
  /// A third quantity would need a third scale nobody can read beside two.
  it("take two quantities and refuse a third, naming the two", () => {
    const hr = listed({});
    const temp = listed({ ...celsius, source_id: "yolink", human_name: "Bedroom" });
    expect(refusal([], hr)).toBeNull();
    expect(refusal([hr], temp)).toBeNull();
    expect(refusal([hr, temp], listed({ name: "max_hr" }))).toBeNull();
    const weight = listed({ unit: "kg", unit_label: "kg", quantity: "M1", unit_factor: 1 });
    expect(refusal([hr, temp], weight)).toMatch(/beats\/min and °C/);
  });

  it("refuse a time series of text, which has no line", () => {
    expect(refusal([], listed({ value_type: "text", human_name: "HRV status" }))).toMatch(
      /HRV status is text/,
    );
  });
});

describe("display units", () => {
  it("show a duration or a distance by its size, and anything else in the first unit", () => {
    const any = listed({});
    expect(displayUnit("T1", any, 40 * 31_557_600)).toEqual({
      label: "yr",
      factor: 1 / 31_557_600,
    });
    expect(displayUnit("T1", any, 3 * 86_400)).toEqual({ label: "d", factor: 1 / 86_400 });
    expect(displayUnit("T1", any, 26_400)).toEqual({ label: "h", factor: 1 / 3600 });
    expect(displayUnit("T1", any, 1_800)).toEqual({ label: "min", factor: 1 / 60 });
    expect(displayUnit("T1", any, 0.045)).toEqual({ label: "ms", factor: 1000 });
    expect(displayUnit("L1", any, 4_200)).toEqual({ label: "km", factor: 1 / 1000 });
    expect(displayUnit("T-1{beats}", any, 0.8)).toEqual({
      label: "beats/min",
      factor: 60,
      offset: 0,
    });
  });

  /// An axis is kept in SI units, so a temperature is in kelvin until it
  /// is printed, and its ticks must still fall on round °C.
  it("print a temperature in its own scale, on round ticks", () => {
    const shown = displayUnit("Θ1", listed(celsius), 300);
    expect(formatTick(293.15, shown)).toBe("20");
    expect(axisTicks(273.15, 303.15, shown).map((t) => formatTick(t, shown))).toEqual([
      "0",
      "10",
      "20",
      "30",
    ]);
  });

  it("name what a time series is in", () => {
    expect(unitText(listed({}))).toBe("beats/min");
    expect(unitText(listed(minutes))).toBe("duration");
    expect(unitText(listed({ value_type: "text" }))).toBe("text");
    expect(unitText(listed({ unit_label: "", quantity: "{score}" }))).toBe("no unit");
  });
});

describe("the list", () => {
  it("keeps what answers every word, grouped by source", () => {
    const all = [
      listed({ human_name: "Resting HR", name: "resting_hr" }),
      listed({ human_name: "Weight", name: "weight" }),
      listed({ source_id: "yolink", human_name: "Bedroom temperature", name: "temperature" }),
    ];
    expect(grouped(all, "").map((g) => [g.source, g.items.length])).toEqual([
      ["riker", 2],
      ["yolink", 1],
    ]);
    expect(grouped(all, "RESTING hr").map((g) => g.items.map((i) => i.human_name))).toEqual([
      ["Resting HR"],
    ]);
    expect(grouped(all, "yolink temp")[0]?.items[0]?.human_name).toBe("Bedroom temperature");
  });

  it("lists a document's time series together, the newest document first", () => {
    const run = (uuid: string, name: string, last: number) =>
      listed({ about_uuid: uuid, about_name: name, time_kind: "instant", last_t: last });
    const all = [
      listed({ human_name: "Resting HR" }),
      { ...run("a", "Morning run · 2369-04-13", 100), human_name: "Heart rate" },
      { ...run("b", "Ride · 2369-04-14", 900), human_name: "Heart rate" },
      { ...run("a", "Morning run · 2369-04-13", 100), human_name: "Speed" },
    ];
    const [g] = grouped(all, "");
    expect(g!.items.map((s) => s.human_name)).toEqual(["Resting HR"]);
    expect(g!.abouts.map((a) => [a.name, a.items.length])).toEqual([
      ["Ride · 2369-04-14", 1],
      ["Morning run · 2369-04-13", 2],
    ]);
    expect(grouped(all, "morning speed")[0]!.abouts[0]!.items.map((s) => s.human_name)).toEqual([
      "Speed",
    ]);
  });
});

describe("line labels", () => {
  /// Five YoLink devices' temperatures were five lines all called "Temperature".
  it("name each line by what tells it apart from the others", () => {
    const at = (id: string, about: string | null, human: string) =>
      listed({ time_series_id: id, about_uuid: about, about_name: about, human_name: human });
    const labels = (picked: Listed[]) => [...lineLabels(picked).values()];
    expect(
      labels([at("a", "Basement freezer", "Temperature"), at("b", "Garage", "Temperature")]),
    ).toEqual(["Basement freezer", "Garage"]);
    expect(labels([at("a", "Morning run", "Heart rate"), at("b", "Morning run", "Speed")])).toEqual(
      ["Heart rate", "Speed"],
    );
    expect(
      labels([at("a", "Garage", "Temperature"), at("b", "Attic", "Relative humidity")]),
    ).toEqual(["Temperature · Garage", "Relative humidity · Attic"]);
    expect(labels([at("a", "Garage", "Temperature")])).toEqual(["Temperature"]);
    expect(
      labels([at("a", null, "Resting HR"), { ...at("b", null, "Resting HR"), source_id: "data" }]),
    ).toEqual(["riker", "data"]);
  });
});

describe("ranges", () => {
  it("end at the newest point picked, and All has no bounds", () => {
    const day = listed({ time_kind: "day", last_t: 10 });
    const instant = listed({ time_kind: "instant", last_t: 5 * 86_400_000 });
    expect(newestMs([day, instant])).toBe(10 * 86_400_000);
    expect(rangeBounds("30d", 100 * 86_400_000)).toEqual({
      fromMs: 70 * 86_400_000,
      toMs: 100 * 86_400_000,
    });
    expect(rangeBounds("all", 1)).toBeNull();
  });
});

describe("lines", () => {
  /// A day is a calendar day, not an instant: it lands on midnight UTC and
  /// reads back as the same date wherever the viewer is.
  it("put a day on its date and a bucket on its mean, with the band beside it, in SI units", () => {
    const raw: Answered = {
      ...listed({ unit_factor: 1 }),
      kind: "raw",
      points: [
        { t: 145_835, value: 48, text: null },
        { t: 145_836, value: null, text: "BALANCED" },
      ],
    };
    expect(lineOf(raw)).toEqual([{ x: toMs("day", 145_835), y: 48 }]);
    expect(formatTime(toMs("day", 145_835), "day", 0)).toBe("2369-04-14");
    const noon = new Date(2026, 8, 3, 12, 5).getTime();
    expect(formatTimeTick(noon, "instant", 3_600_000, true)).toBe("2026-09-03 12:05");
    expect(formatTimeTick(noon, "instant", 3_600_000, false)).toBe("12:05");
    expect(formatTimeTick(noon, "instant", 30 * 86_400_000, false)).toBe("2026-09-03");
    expect(bandOf(raw)).toBeNull();
    const bucketed: Answered = {
      ...listed({ ...minutes, time_kind: "instant" }),
      kind: "buckets",
      width: 10,
      buckets: [{ t: 0, min: 1, max: 3, mean: 2, count: 10 }],
    };
    expect(lineOf(bucketed)).toEqual([{ x: 0, y: 120 }]);
    expect(bandOf(bucketed)).toEqual([{ x: 0, lo: 60, hi: 180 }]);
  });
});

describe("the cursor", () => {
  it("reads the nearest point, and nothing when none is near", () => {
    const line = [
      { x: 0, y: 1 },
      { x: 100, y: 2 },
    ];
    expect(nearest(line, 60)).toEqual({ x: 100, y: 2 });
    expect(nearest(line, 50, 10)).toBeNull();
    expect(nearest([], 0)).toBeNull();
  });
});

describe("scales", () => {
  it("tick at round values and cover the data with a margin", () => {
    expect(niceTicks(0, 100)).toEqual([0, 25, 50, 75, 100]);
    expect(niceTicks(44, 52)).toEqual([44, 46, 48, 50, 52]);
    const [lo, hi] = extent([44, 52])!;
    expect(lo).toBeLessThan(44);
    expect(hi).toBeGreaterThan(52);
    expect(extent([5, 5])).toEqual([4.75, 5.25]);
    expect(extent([])).toBeNull();
    expect(formatTick(50, { label: "", factor: 1 })).toBe("50");
    expect(formatTick(28_800, { label: "h", factor: 1 / 3600 })).toBe("8");
    expect(formatTick(0.1 + 0.2, { label: "", factor: 1 })).toBe("0.3");
  });
});

describe("the card's state", () => {
  it("round-trips the picks and the range, and is empty when pristine", () => {
    expect(encodeState([], "90d")).toBe("");
    const s = encodeState(["a", "b"], "1y");
    expect(decodeState(s)).toEqual({ ids: ["a", "b"], range: "1y" });
    expect(decodeState("range=bogus")).toEqual({ ids: [], range: "90d" });
  });
});
