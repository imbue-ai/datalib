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

  await card.getByRole("button", { name: "Align starts" }).click();
  await expect(card.getByRole("button", { name: "Align starts" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  // Each run is asked for its own half hour, not one span across both. A
  // clock-time call scheduled before the click may still arrive, so this
  // waits for the two aligned calls rather than for exactly two.
  const key = (ids: string, from: number, to: number) => `${ids} ${from} ${to}`;
  await expect
    .poll(() => asked.map((a) => key(a.ids.join(","), a.from, a.to)))
    .toEqual(
      expect.arrayContaining([
        key("tue", TUESDAY, TUESDAY + LENGTH),
        key("sat", SATURDAY, SATURDAY + LENGTH),
      ]),
    );

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

test("the window box takes a preset, a typed duration and two dates", async ({ page }) => {
  await page.addInitScript(() => {
    const view = { zone: { kind: "named", name: "UTC" }, days: "iso", hours: "h23" };
    localStorage.setItem("datalib-time-view", JSON.stringify(view));
  });
  await page.route("**/applet/unified_time_series_index/time_series", (r: Route) =>
    r.fulfill({ json: { time_series: RUNS } }),
  );
  const asked: { from: number; to: number }[] = [];
  await page.route("**/applet/unified_time_series_index/points", async (r: Route) => {
    const body = r.request().postDataJSON() as { ids: string[]; from: number; to: number };
    asked.push(body);
    const time_series = RUNS.filter((s) => body.ids.includes(s.time_series_id)).map((s) => ({
      ...s,
      kind: "raw",
      points: [{ t: s.first_t, value: 120, text: null }],
    }));
    await r.fulfill({ json: { time_series, missing: [], unreadable: [] } });
  });

  await page.goto("/timeSeriesView()");
  const card = page.locator(".ts-card");
  await card.locator(".ts-item", { hasText: "Saturday run" }).locator("input").check();
  const box = card.getByRole("combobox", { name: "Time window" });
  await expect(box).toHaveValue("90d");
  const newest = SATURDAY + LENGTH;
  const askedFor = (from: number, to: number) =>
    expect.poll(() => asked.some((a) => a.from === from && a.to === to)).toBe(true);

  // Focus offers the presets; a click on one applies it.
  await box.focus();
  await expect(card.getByRole("option", { name: /Last 1 hour/ })).toBeVisible();
  // The list floats over the chart rather than pushing it down, and is
  // not a bulleted list: the box once shipped without its styles.
  const list = card.getByRole("listbox", { name: "Time windows" });
  await expect(list).toHaveCSS("position", "absolute");
  await expect(list).toHaveCSS("list-style-type", "none");
  await card.getByRole("option", { name: /Last 15 minutes/ }).click();
  await expect(box).toHaveValue("15m");
  await askedFor(newest - 15 * MIN, newest);

  // A typed duration, applied with Enter.
  await box.fill("45m");
  await box.press("Enter");
  await expect(box).toHaveValue("45m");
  await askedFor(newest - 45 * MIN, newest);

  // Something that is not a window says so and changes nothing.
  await box.fill("soon");
  await expect(card.locator(".tw-error")).toBeVisible();
  await box.press("Escape");
  await expect(box).toHaveValue("45m");

  // Two dates, read in the clock's zone.
  await box.fill("2369-04-18 07:10 to 2369-04-18 07:20");
  await box.press("Enter");
  await askedFor(SATURDAY + 10 * MIN, SATURDAY + 20 * MIN);
});
