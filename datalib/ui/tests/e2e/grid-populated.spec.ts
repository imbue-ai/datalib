import { test, expect } from "@playwright/test";
import { expectGridPainted } from "./grid-helpers";

// Smoke test: the grid actually renders rows from the TNG fixture.
//
// Catches regressions where the harness materializes
// `backend_index.doltlite_db` but the backend looks somewhere else, so
// the grid comes up empty. The e2e harness materializes from the same
// fixture dump `dev_tng` uses, so a green run here means the dev_tng
// path works too.

test("the grid populates with rows from the fixture", async ({
  page,
  request,
}) => {
  // Backend has rows.
  const resp = await request.get("/applet/unified_index/search?q=&limit=50");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: unknown[] };
  expect(data.rows.length, "fixture must have at least one row").toBeGreaterThan(0);

  // Grid surfaces them.
  await page.goto("/");
  const firstRow = page
    .locator('.ag-grid-scrolling-rows [role="row"]')
    .first();
  await expect(firstRow).toBeVisible({ timeout: 10_000 });

  const rowCount = await page
    .locator('.ag-grid-scrolling-rows [role="row"]')
    .count();
  expect(rowCount).toBeGreaterThan(0);

  // …and the grid they're in has real height. Rows stay in the DOM when
  // a grid collapses (the WebKit percentage-height bug —
  // see expectGridPainted), so the count above cannot tell the two
  // apart. This one does not currently reproduce that collapse even
  // with `.grid`'s fix reverted — an absolutely-positioned
  // `.card-app-root` above it makes the percentage resolvable — so this
  // is a forward guard on the surface, not a reproduction of the bug.
  // manager2-grid.spec.ts is the spec that fails without its fix.
  await expectGridPainted(page.locator(".ag-root-wrapper").first(), "Explore grid");
});
