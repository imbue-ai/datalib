import { test, expect, type Route } from "@playwright/test";

// The Time series card's "Align starts": two runs recorded four days
// apart are drawn from their own first point, so they overlap and one
// hover reads both. The spec answers the time-series applet itself, with
// made-up runs, so it knows which stretch each /points call should name.

const MIN = 60_000;
const TUESDAY = Date.parse("2369-04-14T07:00:00Z");
const SATURDAY = TUESDAY + 4 * 24 * 60 * MIN;
const LENGTH = 30 * MIN;

const run = (id: string, start: number, name: string) => ({
  time_series_id: id,
  source_id: "garmin",
  about_uuid: null,
  about_name: null,
  name: `heart_rate_${id}`,
  human_name: name,
  unit: "{beats}/min",
  unit_label: "beats/min",
  quantity: "T-1{beats}",
  unit_factor: 1 / 60,
  unit_offset: 0,
  value_type: "float",
  time_kind: "instant",
  point_count: 31,
  first_t: start,
  last_t: start + LENGTH,
});
const RUNS = [run("tue", TUESDAY, "Tuesday run"), run("sat", SATURDAY, "Saturday run")];

test.use({ viewport: { width: 1600, height: 900 } });

test("aligned starts draw two runs from 0 and one hover reads both", async ({ page }) => {
  await page.route("**/applet/unified_time_series_index/time_series", (r: Route) =>
    r.fulfill({ json: { time_series: RUNS } }),
  );
  const asked: { ids: string[]; from: number; to: number }[] = [];
  await page.route("**/applet/unified_time_series_index/points", async (r: Route) => {
    const body = r.request().postDataJSON() as { ids: string[]; from: number; to: number };
    asked.push(body);
    const time_series = RUNS.filter((s) => body.ids.includes(s.time_series_id)).map((s, i) => {
      const points = [];
      for (let t = s.first_t; t <= s.last_t; t += MIN) {
        if (t >= body.from && t <= body.to) points.push({ t, value: 120 + i * 10, text: null });
      }
      return { ...s, kind: "raw", points };
    });
    await r.fulfill({ json: { time_series, missing: [], unreadable: [] } });
  });

  await page.goto("/timeSeriesView()");
  const card = page.locator(".ts-card");
  for (const name of ["Tuesday run", "Saturday run"]) {
    await card.locator(".ts-item", { hasText: name }).locator("input").check();
  }
  await expect(card.locator(".ts-chart .uplot")).toBeVisible();

  asked.length = 0;
  await card.getByRole("button", { name: "Align starts" }).click();
  await expect(card.getByRole("button", { name: "Align starts" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  // Each run is asked for its own half hour, not one span across both.
  await expect
    .poll(() =>
      asked
        .map((a) => [a.ids.join(","), a.from, a.to])
        .sort()
        .map(String),
    )
    .toEqual(
      [
        ["sat", SATURDAY, SATURDAY + LENGTH],
        ["tue", TUESDAY, TUESDAY + LENGTH],
      ].map(String),
    );
  await expect(card.locator(".ts-range", { hasText: "90 d" })).toBeDisabled();

  // Halfway along the axis is fifteen minutes into both runs.
  const over = card.locator(".u-over");
  const r = await over.evaluate((el) => {
    const b = el.getBoundingClientRect();
    return { x: b.left, y: b.top, w: b.width, h: b.height };
  });
  const at = { clientX: r.x + r.w * 0.5, clientY: r.y + r.h / 2 };
  await over.dispatchEvent("mouseenter", at);
  await over.dispatchEvent("mousemove", { ...at, movementX: 1 });
  await expect(card.locator(".ts-when")).toHaveText("+15:00.000");
  await expect(card.locator(".ts-tooltip > div:not(.ts-when)")).toHaveCount(2);
});
