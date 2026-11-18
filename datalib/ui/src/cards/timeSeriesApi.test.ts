import { afterEach, describe, expect, it, vi } from "vitest";

import type { Listed } from "./timeSeries";
import { fetchAlignedPoints, fetchTimeSeries } from "./timeSeriesApi";

afterEach(() => vi.restoreAllMocks());

function answer(status: number, body: unknown) {
  vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(JSON.stringify(body), { status }));
}

describe("fetchTimeSeries", () => {
  /** A root with no time-series index drew the gateway's raw
   *  `502: {"error":"no applet …"}` inside every Garmin document. */
  it("answers null when the root runs no time-series index", async () => {
    answer(502, { error: 'no applet "unified_time_series_index"' });
    await expect(fetchTimeSeries()).resolves.toBeNull();
  });

  it("still reports any other failure as it came", async () => {
    answer(500, { error: "boom" });
    await expect(fetchTimeSeries()).rejects.toThrow('time series → 500: {"error":"boom"}');
  });
});

describe("fetchAlignedPoints", () => {
  /** Two runs on different days, aligned, must each be asked for their
   *  own minutes; one shared span would miss one of them entirely. */
  it("asks each run for its own stretch and answers from 0", async () => {
    const MIN = 60_000;
    const run = (id: string, start: number) =>
      ({
        time_series_id: id,
        time_kind: "instant",
        first_t: start,
        last_t: start + 30 * MIN,
      }) as Listed;
    const a = run("a", 1_000 * MIN);
    const b = run("b", 9_000 * MIN);
    const asked: { ids: string[]; from: number; to: number }[] = [];
    vi.spyOn(globalThis, "fetch").mockImplementation(async (_url, init) => {
      const body = JSON.parse(String(init!.body)) as { ids: string[]; from: number; to: number };
      asked.push(body);
      const p = body.ids[0] === "a" ? a : b;
      return new Response(
        JSON.stringify({
          time_series: [{ ...p, kind: "raw", points: [{ t: body.from, value: 1, text: null }] }],
        }),
      );
    });
    const got = await fetchAlignedPoints([a, b], { fromMs: 5 * MIN, toMs: 10 * MIN }, 100);
    expect(asked).toEqual([
      { ids: ["a"], from: 1_005 * MIN, to: 1_010 * MIN, budget: 100 },
      { ids: ["b"], from: 9_005 * MIN, to: 9_010 * MIN, budget: 100 },
    ]);
    for (const id of ["a", "b"]) {
      const ans = got.answered.get(id)!;
      expect(ans.kind === "raw" && ans.points[0]!.t).toBe(5 * MIN);
    }
  });
});
