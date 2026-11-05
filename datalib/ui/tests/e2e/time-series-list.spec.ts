import { test, expect, type Route } from "@playwright/test";

// The Time series card's list: a document's time series fold under its
// name. The spec answers the time-series applet itself, with one made-up
// run, since what it checks is the list, not the store.

const START = Date.parse("2369-04-14T07:00:00Z");
const series = (name: string, human: string) => ({
  time_series_id: `run-1/${name}`,
  source_id: "garmin",
  about_uuid: "run-1",
  about_name: "Morning run",
  name,
  human_name: human,
  unit: "{beats}/min",
  unit_label: "beats/min",
  quantity: "T-1{beats}",
  unit_factor: 1 / 60,
  unit_offset: 0,
  value_type: "float",
  time_kind: "instant",
  point_count: 1,
  first_t: START,
  last_t: START + 60_000,
});
const LISTED = [series("heart_rate", "Heart rate"), series("cadence", "Cadence")];

/** Unticking the last pick under a run folded the run the person had
 *  unfolded by hand. */
test("taking a pick off leaves its run unfolded; only a click folds it", async ({ page }) => {
  await page.route("**/applet/unified_time_series_index/time_series", (r: Route) =>
    r.fulfill({ json: { time_series: LISTED } }),
  );
  await page.route("**/applet/unified_time_series_index/points", async (r: Route) => {
    const body = r.request().postDataJSON() as { ids: string[] };
    const time_series = LISTED.filter((s) => body.ids.includes(s.time_series_id)).map((s) => ({
      ...s,
      kind: "raw",
      points: [{ t: START, value: 120, text: null }],
    }));
    await r.fulfill({ json: { time_series, missing: [], unreadable: [] } });
  });

  await page.goto("/timeSeriesView()");
  const run = page.locator(".ts-about", { hasText: "Morning run" });
  const heartRate = run.locator(".ts-item", { hasText: "Heart rate" }).locator("input");
  await expect(run).not.toHaveAttribute("open");

  await run.locator("summary").click();
  await expect(run).toHaveAttribute("open");
  await heartRate.check();
  await expect(page.locator(".ts-chart .uplot")).toBeVisible();
  await heartRate.uncheck();
  await expect(heartRate).not.toBeChecked();
  await expect(run).toHaveAttribute("open");

  await run.locator("summary").click();
  await expect(run).not.toHaveAttribute("open");
});
