import { test, expect } from "@playwright/test";
import { clickRowByUuid } from "./grid-helpers";

// The yolink page is the only rendered document in the tree whose body
// is mostly `<iframe>`s. That makes it the only coverage for a seam
// several pieces have to agree on:
//
//   renderer writes a RELATIVE `src="plots/<q>.html"`  (render/render.rs)
//     → ChatBody rewrites it to `/applet/unified_index/asset/{markdown_uuid}/…`  (asset_urls.ts)
//       → the backend resolves that against the markdown's directory  (http/src/lib.rs)
//         → the framed document is the page the renderer generated
//
// Each piece has its own unit test; none of them notices when the
// contract between two of them changes. Breaking any single link here
// leaves a document that renders an empty box, which no other test sees.
//
// ─── What this deliberately does NOT assert ───────────────────────────
//
// That Plotly *drew*. The plot pages load Plotly from cdn.plot.ly (see
// `render/plot.rs::PLOTLY_SRC`), and asserting on a canvas would make
// this test need the public internet and stay green only while a third
// party is up. Instead we read the figure spec the page inlines and
// assert on that — it is the renderer's output, which is the part we
// own. When the CDN is unreachable the page shows its offline notice,
// which is designed behavior, not a regression.
//
// ─── Trap, if you extend this ────────────────────────────────────────
//
// playwright.config.ts sets `use.extraHTTPHeaders.authorization` for the
// whole browser context, and Playwright applies it to cross-origin
// subresources too. An `Authorization` header makes the cdn.plot.ly
// script fetch a preflighted CORS request, which the CDN answers with a
// non-2xx — so any assertion that needs Plotly to load will fail here
// while working perfectly in the real app (which authenticates with an
// HttpOnly cookie, never that header). Override with
// `test.use({ extraHTTPHeaders: {} })` and a `?token=` navigation if you
// ever need the real library.

type Row = { uuid: string; kind: string; markdown_uuid: string | null };

/** The figure spec `render/plot.rs` inlines into every plot page. */
type Figure = {
  data: { name: string; type: string; y: number[]; yaxis?: string }[];
  layout: { yaxis: { title: { text: string } }; yaxis2?: unknown };
};

test.use({ viewport: { width: 1600, height: 900 } });

/** Scroll the `<name>.html` iframe into view, wait for its frame to
 *  navigate, and return it.
 *
 *  Both halves are load-bearing, and only under WebKit:
 *
 *   * The renderer emits `loading="lazy"` (render/render.rs), so a
 *     frame below the fold does not navigate at all until it nears the
 *     viewport. Chromium's lazy-load threshold reaches far enough down
 *     the page to cover all three 520px-tall plots at this viewport
 *     size; WebKit's does not, and `volume` never loaded. Scrolling is
 *     what a reader does anyway.
 *   * Even for a frame that is loading, the `<iframe>` element existing
 *     does not mean the frame has navigated yet, so `page.frames()` has
 *     to be polled rather than sampled once.
 */
async function plotFrame(
  page: import("@playwright/test").Page,
  iframe: import("@playwright/test").Locator,
  name: string,
) {
  await iframe.scrollIntoViewIfNeeded();
  const matches = () => page.frames().filter((f) => f.url().endsWith(`${name}.html`));
  await expect
    .poll(() => matches().length, {
      message: `the ${name} frame should have loaded`,
      timeout: 10_000,
    })
    .toBeGreaterThan(0);
  const frame = matches()[0];
  await frame.waitForLoadState("domcontentloaded");
  return frame;
}

/** The `<script id="figure" type="application/json">` block a plot page
 *  inlines. Empty is a failure, not an empty figure — it would mean the
 *  frame loaded something that isn't our page. */
async function figureJson(frame: import("@playwright/test").Frame) {
  const text = await frame.locator("#figure").textContent({ timeout: 10_000 });
  expect(text, `${frame.url()} has no inlined figure spec`).toBeTruthy();
  return text!;
}

test("the yolink page's plot iframes resolve to backend asset URLs", async ({
  page,
  request,
}) => {
  const resp = await request.get("/applet/unified_index/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const pageRow = rows.find((r) => r.kind === "Sensor Timeseries");
  expect(pageRow, "the TNG fixture must contain the yolink page row").toBeTruthy();
  const mdUuid = pageRow!.markdown_uuid ?? pageRow!.uuid;

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 15_000 });
  await clickRowByUuid(page, pageRow!.uuid);

  // One iframe per physical quantity the fixture covers, each pointing
  // at the asset route rather than at the renderer's relative path.
  const plotIframe = (quantity: string) =>
    page.locator(
      `iframe[src="/applet/unified_index/asset/${mdUuid}/plots/${quantity}.html"]`,
    );
  for (const quantity of ["temperature", "humidity", "volume"]) {
    await expect(
      plotIframe(quantity),
      `plots/${quantity}.html should be iframed via the asset route`,
    ).toHaveCount(1);
  }
  // No un-rewritten relative src survived.
  await expect(page.locator('iframe[src^="plots/"]')).toHaveCount(0);

  // The frame really loaded the generated page — not a 404, not an
  // error document.
  const temperature = await plotFrame(page, plotIframe("temperature"), "temperature");
  expect(await temperature.title()).toBe("Temperature");

  const figure: Figure = JSON.parse(await figureJson(temperature));

  // Every temperature-capable device is its own series on the one plot.
  expect(figure.data.map((t) => t.name)).toEqual([
    "sickbay_plasma_fridge",
    "stasis_unit_alpha",
    "ten_forward_cooler",
  ]);
  expect(figure.layout.yaxis.title.text).toBe("°C");
  expect(figure.layout.yaxis2, "temperature is a single-axis plot").toBeUndefined();

  // `sickbay_plasma_fridge` stores `temperature_f`; it must arrive here
  // in °C. Its raw values sit around 44.6°F, so a dropped conversion
  // shows up as a series in the 40s rather than the single digits — and
  // as a device that no longer shares an axis with the other two.
  const sickbay = figure.data.find((t) => t.name === "sickbay_plasma_fridge")!;
  expect(Math.min(...sickbay.y)).toBeGreaterThan(4);
  expect(Math.max(...sickbay.y)).toBeLessThan(10);

  // The volume plot is the two-axis case: per-sample consumption on the
  // left, the lifetime totalizer overlaid on the right.
  const volume = await plotFrame(page, plotIframe("volume"), "volume");
  const volFigure: Figure = JSON.parse(await figureJson(volume));
  expect(volFigure.layout.yaxis2).toBeDefined();
  const total = volFigure.data.find((t) =>
    t.name.includes("meter total"),
  )!;
  expect(total.yaxis).toBe("y2");
  // Gallons upstream, litres on the plot: the fixture's meter starts at
  // 8100 gal, which is ~30665 L. Unconverted it would still read ~8100.
  expect(total.y[0]).toBeGreaterThan(30_000);
});
