import { describe, expect, it } from "vitest";

import {
  bandOf,
  decodeState,
  displayUnit,
  encodeState,
  extent,
  formatTick,
  formatTime,
  grouped,
  lineOf,
  nearest,
  newestMs,
  niceTicks,
  rangeBounds,
  refusal,
  toMs,
  type Answered,
  type Listed,
} from "./timeSeries";

function listed(over: Partial<Listed>): Listed {
  return {
    time_series_id: "id",
    source_id: "riker",
    entity: "account",
    metric: "resting_hr",
    label: "Resting HR",
    unit: "bpm",
    time_kind: "day",
    point_count: 1,
    first_t: 0,
    last_t: 0,
    ...over,
  };
}

describe("the axes", () => {
  /// A third unit would need a third scale nobody can read beside two.
  it("take two units and refuse a third, naming the two", () => {
    const hr = listed({ unit: "bpm" });
    const temp = listed({ unit: "°C", source_id: "yolink", label: "Bedroom" });
    expect(refusal([], hr)).toBeNull();
    expect(refusal([hr], temp)).toBeNull();
    expect(refusal([hr, temp], listed({ unit: "bpm", metric: "max_hr" }))).toBeNull();
    expect(refusal([hr, temp], listed({ unit: "kg" }))).toMatch(/bpm and °C/);
  });
});

describe("display units", () => {
  it("show long durations in hours, short ones in minutes, and long distances in km", () => {
    expect(displayUnit("s", 26_400)).toEqual({ label: "h", factor: 1 / 3600 });
    expect(displayUnit("s", 1_800)).toEqual({ label: "min", factor: 1 / 60 });
    expect(displayUnit("s", 0.045)).toEqual({ label: "ms", factor: 1000 });
    expect(displayUnit("m", 4_200)).toEqual({ label: "km", factor: 1 / 1000 });
    expect(displayUnit("bpm", 48)).toEqual({ label: "bpm", factor: 1 });
  });
});

describe("the list", () => {
  it("keeps what answers every word, grouped by source", () => {
    const all = [
      listed({ label: "Resting HR", metric: "resting_hr" }),
      listed({ label: "Weight", metric: "weight" }),
      listed({ source_id: "yolink", label: "Bedroom temperature", metric: "temperature" }),
    ];
    expect(grouped(all, "").map((g) => [g.source, g.items.length])).toEqual([
      ["riker", 2],
      ["yolink", 1],
    ]);
    expect(grouped(all, "RESTING hr").map((g) => g.items.map((i) => i.label))).toEqual([
      ["Resting HR"],
    ]);
    expect(grouped(all, "yolink temp")[0]?.items[0]?.label).toBe("Bedroom temperature");
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
  it("put a day on its date and a bucket on its mean, with the band beside it", () => {
    const raw: Answered = {
      ...listed({}),
      kind: "raw",
      points: [
        { t: 145_835, value: 48, text: null },
        { t: 145_836, value: null, text: "BALANCED" },
      ],
    };
    expect(lineOf(raw)).toEqual([{ x: toMs("day", 145_835), y: 48 }]);
    expect(formatTime(toMs("day", 145_835), "day", 0)).toBe("2369-04-14");
    expect(bandOf(raw)).toBeNull();
    const bucketed: Answered = {
      ...listed({ time_kind: "instant" }),
      kind: "buckets",
      width: 10,
      buckets: [{ t: 0, min: 1, max: 3, mean: 2, count: 10 }],
    };
    expect(lineOf(bucketed)).toEqual([{ x: 0, y: 2 }]);
    expect(bandOf(bucketed)).toEqual([{ x: 0, lo: 1, hi: 3 }]);
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
    expect(formatTick(50, { factor: 1 })).toBe("50");
    expect(formatTick(28_800, { factor: 1 / 3600 })).toBe("8");
    expect(formatTick(0.1 + 0.2, { factor: 1 })).toBe("0.3");
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
