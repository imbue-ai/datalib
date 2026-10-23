import { afterEach, describe, expect, it, vi } from "vitest";

import { fetchTimeSeries } from "./timeSeriesApi";

afterEach(() => vi.restoreAllMocks());

function answer(status: number, body: unknown) {
  vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(JSON.stringify(body), { status }));
}

describe("fetchTimeSeries", () => {
  /** A root with no time-series index drew the gateway's raw
   *  `502: {"error":"no applet …"}` inside every Garmin document. */
  it("says in words that the root runs no time-series index", async () => {
    answer(502, { error: 'no applet "unified_time_series_index"' });
    const e = await fetchTimeSeries().then(
      () => null,
      (e: Error) => e,
    );
    expect(e?.message).toBe("This data root does not keep a time-series index, so no chart.");
  });

  it("still reports any other failure as it came", async () => {
    answer(500, { error: "boom" });
    await expect(fetchTimeSeries()).rejects.toThrow('time series → 500: {"error":"boom"}');
  });
});
