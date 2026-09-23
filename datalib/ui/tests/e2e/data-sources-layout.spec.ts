// What a person does to the sources card's columns outlives a sync.
//
// While a run goes, the card re-reads its rows several times a second
// and every answer reaches the grid. That has to be an edit of the rows,
// not a redraw of the grid: a dragged width, a header sort, stay put.
// A sync writes the root's stores and job queue, so this spec has a root
// of its own (`CONFIG_MUTATING`).

import { test, expect, type Page } from "@playwright/test";
import { TABLE_ROWS } from "./grid-helpers";

const header = (page: Page, colId: string) =>
  page.locator(`.tg-grid .slick-header-column[col-id="${colId}"]`);

async function openSources(page: Page) {
  await page.goto("/data_sources");
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await expect(page.locator(TABLE_ROWS)).not.toHaveCount(0, { timeout: 10_000 });
}

/// Run a whole sync from the card and wait for it to end — without the
/// reload `settleRunner` does, which would throw away the layout under
/// test. Waits until the run has closed and the card has read its rows
/// at least twice in the meantime, so the grid really was handed new
/// answers.
async function syncEverything(page: Page) {
  let reads = 0;
  page.on("response", (r) => {
    if (r.url().includes("/api/manage/rows") && r.ok()) reads++;
  });
  await page.getByRole("button", { name: "Sync everything" }).click();
  await expect(page.getByText("Queued a sync of everything.")).toBeVisible();
  await expect
    .poll(
      async () => {
        const dag = await (await page.request.get("/api/dag")).json();
        return dag.run?.live !== true && reads >= 2;
      },
      { timeout: 60_000, intervals: [200], message: "the sync never ended" },
    )
    .toBe(true);
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
  const type = header(page, "type");
  const before = (await type.boundingBox())!.width;
  // Well under the column's declared 120px, which used to be its floor.
  const grip = (await type.locator(".slick-resizable-handle").boundingBox())!;
  const x = grip.x + grip.width / 2;
  const y = grip.y + grip.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x - 60, y, { steps: 5 });
  await page.mouse.up();
  const dragged = (await type.boundingBox())!.width;
  expect(dragged).toBeLessThan(before - 40);
  expect(dragged).toBeLessThan(100);

  await syncEverything(page);
  expect((await type.boundingBox())!.width).toBe(dragged);
});

test("a header sort outlives a sync", async ({ page }) => {
  test.setTimeout(120_000);
  await openSources(page);
  const declared = await groupOrder(page);
  await header(page, "type").click();
  // The sort is on screen: the order moved off the config's.
  await expect.poll(() => groupOrder(page)).not.toEqual(declared);
  const sorted = await groupOrder(page);

  await syncEverything(page);
  expect(await groupOrder(page)).toEqual(sorted);
});
