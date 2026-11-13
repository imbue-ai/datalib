// Muting problem rows a person has seen: select them in the problems grid
// the Dashboard's "Review" opens, right-click, Mute. The rows stay in the
// grid, marked, and the source's count, and so its line in "Needs you",
// goes. Writes the mutes store and syncs, so it runs on a root of its own
// (`config-mutating.ts`).

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  cardOf,
  gridSettled,
  searchMenuItem,
  selectRowByUuid,
  SEARCH_MENU,
  type GridApi,
} from "./grid-helpers";

const PROBLEMS = "/applet/unified_document_index/problems";

type Problem = {
  problem_uuid: string;
  source_id: string;
  severity: string;
  stage: string;
  muted: string;
};
type SyncRequest = { id: string; roots: string[]; state: string };

async function problems(request: APIRequestContext, q: string): Promise<Problem[]> {
  const r = await request.get(`${PROBLEMS}?q=${encodeURIComponent(q)}&limit=2000`);
  expect(r.ok()).toBe(true);
  return ((await r.json()) as { rows: Problem[] }).rows;
}

const ids = (rows: Problem[]) => rows.map((p) => p.problem_uuid).sort();

const isSelected = (page: Page, uuid: string) =>
  page.evaluate(
    (u) => (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.isSelected(u),
    uuid,
  );

const needsYou = (page: Page) => page.locator('section[aria-label="Needs you"]');
const lineOf = (page: Page, source: string) =>
  needsYou(page)
    .locator(".row")
    .filter({ has: page.locator(`[data-entity="datalib:group/${source}"]`) });

/// Of the sources whose render could not read a record, the one with the
/// fewest such problems, so its every problem is loaded and selected at
/// once. A record that would not parse is found again on every render.
async function quietestRendered(request: APIRequestContext): Promise<string> {
  const counted = new Map<string, number>();
  for (const p of await problems(request, "stage:parse -severity:info")) {
    counted.set(p.source_id, (counted.get(p.source_id) ?? 0) + 1);
  }
  const sources = [...counted.entries()].sort((a, b) => a[1] - b[1]);
  expect(sources.length, "the fixture has a source whose render found problems").toBeGreaterThan(0);
  return sources[0][0];
}

/// Sync the source's render, which reports its problem counts, and wait
/// for the request to close: the fixture root's run store has no counts.
async function render(request: APIRequestContext, source: string) {
  const step = `${source}/render_markdown`;
  const opened = await request.post("/api/requests", { data: { roots: [step] } });
  expect(opened.ok(), await opened.text()).toBe(true);
  const [mine] = (await opened.json()) as SyncRequest[];
  await expect
    .poll(
      async () =>
        ((await (await request.get("/api/requests")).json()) as SyncRequest[]).find(
          (r) => r.id === mine.id,
        )?.state,
      { timeout: 120_000, message: `the sync of ${step} never closed` },
    )
    .toBe("done");
}

let muted: string[] = [];

test.afterEach(async ({ request }) => {
  if (muted.length === 0) return;
  await request.post(`${PROBLEMS}/mute`, { data: { problem_uuids: muted, muted: false } });
  muted = [];
});

test("muting a source's problems in the grid takes it off Needs you", async ({ page, request }) => {
  // A render and the index after it run first.
  test.setTimeout(180_000);
  const source = await quietestRendered(request);
  await render(request, source);
  const theirs = await problems(request, `source:${source}`);
  // Rendered again unchanged, every problem has the id it had: a mute
  // keyed on the id holds across syncs.
  await render(request, source);
  expect(ids(await problems(request, `source:${source}`))).toEqual(ids(theirs));
  expect(theirs.every((p) => p.muted === "no")).toBe(true);

  await page.goto("/");
  await expect(lineOf(page, source)).toHaveCount(1, { timeout: 15_000 });
  const heading = needsYou(page).locator(".section-title");
  const listed = await needsYou(page).locator(".row").count();
  await expect(heading).toHaveText(new RegExp(`· ${listed}$`));

  await lineOf(page, source).getByRole("button", { name: "Review" }).click();
  const grid = cardOf(page, "gridView(");
  await expect(grid).toBeVisible({ timeout: 10_000 });
  await expect(page.getByTestId("grid-search-input")).toHaveAttribute(
    "data-query",
    `source:${source}`,
  );
  await grid.locator(".grid-box .slick-row").first().waitFor({ timeout: 15_000 });
  await gridSettled(page);
  const held = (await page.evaluate(() =>
    (window as unknown as { __fwGridApi: GridApi }).__fwGridApi.rows(),
  )) as Problem[];
  expect(ids(held)).toEqual(ids(theirs));

  // Every row selected: the first by a click, the rest by ⌘/Ctrl-click.
  const [first, ...rest] = theirs;
  await selectRowByUuid(page, first.problem_uuid);
  for (const p of rest) {
    await actOnRowByUuid(page, p.problem_uuid, async (row) => {
      if (await isSelected(page, p.problem_uuid)) return;
      await row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 });
      await expect.poll(() => isSelected(page, p.problem_uuid), { timeout: 1_000 }).toBe(true);
    });
  }

  await actOnRowByUuid(page, first.problem_uuid, (row) =>
    row.click({ button: "right", position: { x: 40, y: 10 }, timeout: 3_000 }),
  );
  await expect(page.locator(SEARCH_MENU)).toBeVisible({ timeout: 5_000 });
  const mute = new RegExp(`^Mute${theirs.length === 1 ? "" : ` \\(${theirs.length}\\)`}$`);
  muted = ids(theirs);
  await searchMenuItem(page, mute).click();

  // The store has them, and the rows stay, each marked.
  await expect
    .poll(async () => ids(await problems(request, `source:${source} muted:yes`)))
    .toEqual(ids(theirs));
  await actOnRowByUuid(
    page,
    first.problem_uuid,
    (row) => expect(row.locator('[col-id="muted_chip"]')).toHaveText("MUTED", { timeout: 3_000 }),
    "muted_chip",
  );

  // The Dashboard, back in front, no longer lists the source. Its library
  // section is the proof the Dashboard is what is on screen.
  await page.getByRole("treeitem", { name: /^Dashboard/ }).click();
  await expect(page.getByRole("region", { name: "Your library" })).toBeVisible();
  await expect(lineOf(page, source)).toHaveCount(0, { timeout: 15_000 });
  if (listed > 1) await expect(heading).toHaveText(new RegExp(`· ${listed - 1}$`));
  else await expect(needsYou(page)).toHaveCount(0);

  // Unmuted through the gateway, it is back, on the page as it stands:
  // the mute is a commit the app hears, and the counts are read again.
  await request.post(`${PROBLEMS}/mute`, { data: { problem_uuids: muted, muted: false } });
  muted = [];
  await expect(lineOf(page, source)).toHaveCount(1, { timeout: 15_000 });
});
