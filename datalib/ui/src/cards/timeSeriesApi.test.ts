import { afterEach, describe, expect, it, vi } from "vitest";

import { fetchTimeSeries } from "./timeSeriesApi";

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
