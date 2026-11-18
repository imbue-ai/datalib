// What a person does to the sources card's columns outlives a sync.
//
// While a run goes, the card re-reads its rows several times a second
// and every answer reaches the grid. That has to be an edit of the rows,
// not a redraw of the grid: a dragged width, a header sort, stay put.
// A sync writes the root's stores, so this spec has a root
// of its own (`CONFIG_MUTATING`).

import { test, expect, type Page } from "@playwright/test";
import { TABLE_ROWS, expectSyncEverything } from "./grid-helpers";

const header = (page: Page, colId: string) =>
  page.locator(`.tg-grid .slick-header-column[col-id="${colId}"]`);

async function openSources(page: Page) {
  await page.goto("/data_sources");
  await expectSyncEverything(page);
  await expect(page.locator(TABLE_ROWS)).not.toHaveCount(0, { timeout: 10_000 });
}

/// Run a sync and wait for it to end — without the reload
/// `settleRunner` does, which would throw away the layout under test.
/// Waits on the request itself (the banner goes when it closes, which
/// can be before anything looks for it) and on the card having read its
/// rows meanwhile, so the grid really was handed new answers. A sync
/// this short is one read: the card reads its rows one request at a
/// time (`refreshRows` in `sourcesCardModel.ts`), so a burst of changes
/// lands in one answer.
///
/// One source, opened the way the card's buttons open one. A sync of
/// every source on this root runs every render, the index and every
/// embed, which on a busy runner outlasts any wait here. Garmin's render
/// is over in a second: the materialized root holds no raw store, so it
/// fails, and its row's status and time change all the same.
async function syncOneStep(page: Page) {
  type Request = { id: string; state: string };
  const requests = async () =>
    (await (await page.request.get("/api/requests")).json()) as Request[];
  const before = new Set((await requests()).map((r) => r.id));
  let reads = 0;
  page.on("response", (r) => {
    if (r.url().includes("/api/manage/rows") && r.ok()) reads++;
  });
  const opened = await page.request.post("/api/requests", {
    data: { roots: ["garmin/render_time_series"] },
  });
  expect(opened.ok(), await opened.text()).toBe(true);
  await expect
    .poll(
      async () => {
        const mine = (await requests()).filter((r) => !before.has(r.id));
        const ended = mine.length > 0 && mine.every((r) => r.state !== "open");
        return ended && reads >= 1
          ? "ended"
          : `requests: ${JSON.stringify(mine.map((r) => r.state))}, rows read ${reads} times`;
      },
      { timeout: 60_000, intervals: [200], message: "the sync never ended" },
    )
    .toBe("ended");
  // The last answer is painted a frame after it arrives.
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => r(null))));
}

/// The order the top-level rows are drawn in, by key.
async function groupOrder(page: Page): Promise<string[]> {
  const rows = await page.locator(TABLE_ROWS).evaluateAll((els) =>
    els.map((r) => ({
      key: (r as HTMLElement).dataset.key ?? "",
      top: r.getBoundingClientRect().top,
    })),
  );
  return rows
    .sort((a, b) => a.top - b.top)
    .map((r) => r.key)
    .filter((k) => k.startsWith("group:"));
}

test("a dragged column width outlives a sync", async ({ page }) => {
  test.setTimeout(120_000);
  await openSources(page);
  // Near the left, so its grip is on screen at Playwright's 1280px.
  const col = header(page, "status");
  const before = (await col.boundingBox())!.width;
  const grip = (await col.locator(".slick-resizable-handle").boundingBox())!;
  const x = grip.x + grip.width / 2;
  const y = grip.y + grip.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x - 60, y, { steps: 5 });
  await page.mouse.up();
  const dragged = (await col.boundingBox())!.width;
  expect(dragged).toBeLessThan(before - 40);

  await syncOneStep(page);
  expect((await col.boundingBox())!.width).toBe(dragged);
});

/// The config's order is a hidden column, so without a way back a
/// header click would lose it until the card was reopened.
test("a third click on a header puts the config's order back", async ({ page }) => {
  await openSources(page);
  const declared = await groupOrder(page);
  const name = header(page, "name");
  await name.click();
  await expect.poll(() => groupOrder(page)).not.toEqual(declared);
  const ascending = await groupOrder(page);
  await name.click();
  await expect.poll(() => groupOrder(page)).not.toEqual(ascending);
  await name.click();
  await expect.poll(() => groupOrder(page)).toEqual(declared);
  await expect(name.locator(".slick-sort-indicator-asc, .slick-sort-indicator-desc")).toHaveCount(
    0,
  );
});

test("a header sort outlives a sync", async ({ page }) => {
  test.setTimeout(120_000);
  await openSources(page);
  const declared = await groupOrder(page);
  await header(page, "name").click();
  // The sort is on screen: the order moved off the config's.
  await expect.poll(() => groupOrder(page)).not.toEqual(declared);
  const sorted = await groupOrder(page);

  await syncOneStep(page);
  expect(await groupOrder(page)).toEqual(sorted);
});
