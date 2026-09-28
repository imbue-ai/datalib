// Grouping the search grid groups on the server (grid/serverGroups.ts):
// every group is there at once with its true count, and a group's rows
// are read only once it is on screen. The fixture has more rows than a
// page, so the counts cannot come from the rows the grid holds.

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { gridSettled, SEARCH_ROWS, type GridApi } from "./grid-helpers";

type Group = { values: (string | null)[]; count: number };

async function groupsOf(request: APIRequestContext, by: string): Promise<Group[]> {
  const r = await request.get(`/applet/unified_index/search/groups?q=&by=${by}`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { groups: Group[] }).groups;
}

async function groupBy(page: Page, ids: string[]) {
  await page.evaluate(
    (ids) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.groupBy(ids),
    ids,
  );
}

/// The group rows the grid draws, as "Type: Chat (12)" → ["Chat", 12].
const titles = (page: Page) =>
  page.locator(".grid-box .slick-group .slick-group-title").evaluateAll((els) =>
    els.map((el) => {
      const m = /^[^:]+: (.*) \((\d+)\)$/.exec((el.textContent ?? "").trim());
      return m ? [m[1], Number(m[2])] : null;
    }),
  );

test("every group is there with its true count before its rows are read", async ({
  page,
  request,
}) => {
  const byKind = await groupsOf(request, "kind");
  expect(byKind.length).toBeGreaterThan(1);
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  // No group's rows arrive: every request for them is held.
  await page.route(/\/search\?.*within=/, () => {});
  await groupBy(page, ["kind"]);

  const count = new Map(byKind.map((g) => [g.values[0] ?? "—", g.count]));
  await expect.poll(async () => (await titles(page)).length).toBeGreaterThan(1);
  for (const t of await titles(page)) {
    expect(t, "a group row the grid drew").not.toBeNull();
    const [value, n] = t as [string, number];
    expect(n, `the group ${value}`).toBe(count.get(value));
  }
  const held = await page.evaluate(
    () => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows().length,
  );
  expect(held, "the counts came from rows the grid had read").toBe(0);
});

/// Opening a group reads its rows, and only its: the rows under it all
/// share its value.
test("opening a group reads its rows", async ({ page, request }) => {
  const [smallest] = (await groupsOf(request, "kind")).sort((a, b) => a.count - b.count);
  await page.goto("/");
  await page.locator(SEARCH_ROWS).first().waitFor({ timeout: 15_000 });
  await groupBy(page, ["kind"]);
  await gridSettled(page);
  await page.locator(".grid-box .slick-group-toggle-all").click();

  const kind = smallest.values[0]!;
  const group = page.locator(".grid-box .slick-group").filter({ hasText: `Type: ${kind} (` });
  await group.locator(".slick-group-toggle").click();
  await expect
    .poll(() =>
      page.evaluate(
        (kind) =>
          (window as unknown as { __fwGridApi: GridApi }).__fwGridApi
            .rows()
            .filter((r) => r.kind === kind).length,
        kind,
      ),
    )
    .toBe(smallest.count);
  await expect(page.locator(".grid-box .datalib-more")).toHaveCount(0);
});
