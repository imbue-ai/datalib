import { test, expect } from "@playwright/test";

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
  const resp = await request.get("/api/search?q=&limit=50");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: unknown[] };
  expect(data.rows.length, "fixture must have at least one row").toBeGreaterThan(0);

  // Grid surfaces them.
  await page.goto("/");
  const firstRow = page
    .locator('.ag-center-cols-container [role="row"]')
    .first();
  await expect(firstRow).toBeVisible({ timeout: 10_000 });

  const rowCount = await page
    .locator('.ag-center-cols-container [role="row"]')
    .count();
  expect(rowCount).toBeGreaterThan(0);
});
