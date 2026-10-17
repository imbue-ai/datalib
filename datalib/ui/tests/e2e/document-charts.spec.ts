import { test, expect, type Route } from "@playwright/test";
import { GRID, selectRowByUuid, docBody } from "./grid-helpers";

// A rendered document asks for a chart with `<div data-time-series="…">`
// and the document view draws it from the time-series applet
// (docs/dev/time_series.md §"Charts in documents"). The TNG fixture
// renders the yolink page but builds no time-series store, so this spec
// answers the applet's two calls itself, with made-up values for the
// ids the page names: what it checks is that the chart is mounted in
// the document's frame and draws, not what the store holds.

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
  await page.route("**/applet/unified_time_series_index/points", async (r: Route) => {
    const asked = (r.request().postDataJSON() as { ids: string[] }).ids;
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
  const drawn = doc.locator("[data-time-series] .ts-chart svg");
  await expect(drawn).toHaveCount(3);
  await expect(drawn.first().locator("polyline")).toHaveCount(charts[0]!.length);
  await expect(drawn.first().locator("text.ts-unit")).toHaveText("°C");
  await expect(doc.locator("[data-time-series] .ts-embed-note")).toHaveText(
    "This chart's time series are not in the time-series index yet.",
  );
});
