import { test, expect, type Route } from "@playwright/test";
import { GRID, selectRowByUuid, docBody } from "./grid-helpers";

// A rendered document asks for a chart with `<div data-time-series="…">`
// and the document view draws it from the time-series applet
// (docs/dev/time_series.md §"Charts in documents"). The TNG fixture
// renders the yolink page but builds no time-series store, so this spec
// answers the applet's two calls itself, with made-up values for the
// ids the page names: what it checks is that the chart is mounted in
// the document's frame and draws, that its legend turns lines off and
// on, and that a zoom asks the applet for the narrower range — not what
// the store holds.

type Row = { uuid: string; kind: string; source_id: string; markdown_uuid: string | null };

const listed = (id: string, i: number) => ({
  time_series_id: id,
  source_id: "yolink",
  about_uuid: null,
  about_name: null,
  name: `sensor_${i}`,
  human_name: `Sensor ${i}`,
  unit: "Cel",
  unit_label: "°C",
  quantity: "Θ1",
  unit_factor: 1,
  unit_offset: 273.15,
  value_type: "float",
  time_kind: "instant",
  point_count: 3,
  first_t: 1_000_000,
  last_t: 3_000_000,
});

/// What a chart's plot says it draws (`data-shown`, plot/usePlot.ts): a
/// digit per line, then two per line for its band's edges.
const drawnAs = (lines: boolean[]) =>
  [...lines, ...lines.flatMap((on) => [on, on])].map((on) => (on ? "1" : "0")).join("");

test.use({ viewport: { width: 1600, height: 900 } });

test("a document's charts are drawn in its frame from the time-series applet", async ({
  page,
  request,
}) => {
  const resp = await request.get("/applet/unified_document_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const pageRow = rows.find((r) => r.kind === "Sensor Timeseries" && r.source_id === "yolink");
  expect(pageRow, "the TNG fixture must contain the yolink page row").toBeTruthy();
  const mdUuid = pageRow!.markdown_uuid ?? pageRow!.uuid;

  const chat = await request.get(`/applet/unified_document_index/chat/${mdUuid}`);
  const body = ((await chat.json()) as { body: string }).body;
  const charts = [...body.matchAll(/data-time-series="([^"]*)"/g)].map((m) => m[1]!.split(" "));
  // Temperature, humidity, and volume as two charts: consumption, and the
  // meter's running total, which would flatten consumption on one axis.
  expect(charts).toHaveLength(4);
  expect(body).not.toContain("<iframe");
  const ids = charts.flat();
  // One chart's time series are not in the index: it says so.
  const missing = new Set(charts[1]);

  await page.route("**/applet/unified_time_series_index/time_series", (r: Route) =>
    r.fulfill({
      json: { time_series: ids.filter((id) => !missing.has(id)).map(listed) },
    }),
  );
  const spans: { from: number; to: number }[] = [];
  await page.route("**/applet/unified_time_series_index/points", async (r: Route) => {
    const body = r.request().postDataJSON() as { ids: string[]; from: number; to: number };
    spans.push({ from: body.from, to: body.to });
    const asked = body.ids;
    await r.fulfill({
      json: {
        time_series: asked.map((id, i) => ({
          ...listed(id, i),
          kind: "raw",
          points: [
            { t: 1_000_000, value: 3 + i, text: null },
            { t: 2_000_000, value: 4 + i, text: null },
            { t: 3_000_000, value: 3.5 + i, text: null },
          ],
        })),
        missing: [],
        unreadable: [],
      },
    });
  });

  await page.goto(GRID);
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 15_000 });
  await selectRowByUuid(page, pageRow!.uuid);

  const doc = docBody(page);
  await expect(doc.locator("[data-time-series]")).toHaveCount(4, { timeout: 10_000 });
  // Every chart whose time series are listed draws a line per series,
  // on an axis in the unit the applet reads.
  const drawn = doc.locator("[data-time-series] .ts-chart .uplot");
  await expect(drawn).toHaveCount(3);
  const first = doc.locator("[data-time-series]").first();
  await expect(first.locator(".plot-legend-entry")).toHaveCount(charts[0]!.length);
  await expect(first.locator(".ts-unit").first()).toHaveText("°C");
  await expect(doc.locator("[data-time-series] .ts-embed-note")).toHaveText(
    "This chart's time series are not in the time-series index yet.",
  );

  // The legend: a click turns a line off and on, a double-click shows it
  // alone and the next shows them all. `data-shown` is what the plot
  // itself drew, not what the legend asked for.
  const multi = charts.findIndex((ids, i) => !missing.has(ids[0]!) && ids.length >= 2 && i >= 0);
  expect(multi, "the yolink page must have a chart of two time series or more").toBeGreaterThan(-1);
  const n = charts[multi]!.length;
  const chart = doc.locator("[data-time-series]").nth(multi);
  const entries = chart.locator(".plot-legend-entry");
  const plot = chart.locator(".ts-plot");
  const all = Array.from({ length: n }, () => true);
  const only = (k: number) => all.map((_, i) => i === k);
  await expect(plot).toHaveAttribute("data-shown", drawnAs(all));
  await entries.nth(0).click();
  await expect(entries.nth(0)).toHaveAttribute("aria-pressed", "false");
  await expect(plot).toHaveAttribute("data-shown", drawnAs(all.map((_, i) => i !== 0)));
  await entries.nth(0).click();
  await expect(plot).toHaveAttribute("data-shown", drawnAs(all));
  await entries.nth(1).dblclick();
  await expect(plot).toHaveAttribute("data-shown", drawnAs(only(1)));
  await entries.nth(1).dblclick();
  await expect(plot).toHaveAttribute("data-shown", drawnAs(all));

  // A drag along x zooms in, and the chart asks the applet for that
  // stretch again. The drag ends with a mouseup in the document's frame,
  // which uPlot does not hear by itself (usePlot.ts::inFrame). The drag
  // is dispatched rather than driven with `page.mouse`: Playwright's
  // WebKit sends mousemoves with `movementX` 0, which uPlot drops mid-drag
  // as a phantom move, so a driven drag never selects anything there.
  const whole = { from: 1_000_000, to: 3_000_000 };
  const over = chart.locator(".u-over");
  const r = await over.evaluate((el) => {
    const b = el.getBoundingClientRect();
    return { x: b.left, y: b.top, w: b.width, h: b.height };
  });
  const at = (f: number) => ({ clientX: r.x + r.w * f, clientY: r.y + r.h / 2, button: 0 });
  await over.dispatchEvent("mouseenter", at(0.3));
  await over.dispatchEvent("mousedown", { ...at(0.3), buttons: 1 });
  for (const f of [0.4, 0.5, 0.6]) {
    await over.dispatchEvent("mousemove", { ...at(f), buttons: 1, movementX: 10 });
  }
  await over.dispatchEvent("mouseup", at(0.6));
  await expect
    .poll(() => spans.some((s) => s.from > whole.from && s.to < whole.to), {
      message: "a zoom should ask /points for a range inside the whole one",
    })
    .toBe(true);
});
