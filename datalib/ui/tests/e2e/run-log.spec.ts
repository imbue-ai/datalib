// The run-log panel: the Manage screen's "Server log" opens the app
// server's own lines in a grid, a right-click on a cell narrows the
// query to that cell's value (and clears it again), and the bar above
// the grid groups the lines by a column.
//
// The grid is built straight on the vanilla SlickGrid bundle, like the
// cards' grids; this is the one place its menu, grouping bar and query
// round-trip are exercised end to end.

import { test, expect, type Page } from "@playwright/test";
import { menuEntry } from "./grid-helpers";

// The commit playwright.config.ts handed the backends. Node's globals
// are not in this tsconfig, as in api-token.spec.ts.
declare const process: { env: Record<string, string | undefined> };
const GIT_HASH = process.env.DATALIB_GIT_HASH;

const ROWS = ".rl-grid .slick-row:not(.slick-group)";

async function openServerLog(page: Page) {
  await page.goto("/sources2");
  await page.getByRole("button", { name: "Server log" }).click();
  const dialog = page.getByRole("dialog", { name: "Server log" });
  await expect(dialog).toBeVisible();
  await expect(dialog.locator(ROWS).first()).toBeVisible({ timeout: 10_000 });
  return dialog;
}

const lineCount = (page: Page) =>
  page
    .locator(".rl-count")
    .evaluate((el) => Number(/(\d+) line/.exec(el.textContent ?? "")?.[1] ?? NaN));

test("a cell's right-click keeps only its value, and the query clears again", async ({ page }) => {
  const dialog = await openServerLog(page);
  const query = dialog.locator(".rl-search");
  await expect(query).toHaveValue("process:http min_level:info");
  const all = await lineCount(page);
  expect(all).toBeGreaterThan(1);

  // The server's boot lines all come from its main thread but one; the
  // menu names the value under the click.
  const mainCell = dialog.locator(`${ROWS} .slick-cell[col-id="thread"]`).filter({ hasText: /^main$/ }).first();
  await mainCell.click({ button: "right" });
  await expect(menuEntry(page, "Keep only Thread=main")).toBeVisible();
  await expect(menuEntry(page, "Exclude all Thread=main")).toBeVisible();
  await menuEntry(page, "Keep only Thread=main").click();

  await expect(query).toHaveValue("process:http min_level:info thread:main");
  // A reload empties the count before it refills, so "fewer than all"
  // alone is met mid-way; wait for the narrowed lines to be there.
  await expect.poll(async () => {
    const n = await lineCount(page);
    return n > 0 && n < all;
  }).toBe(true);
  await expect
    .poll(async () => {
      const threads = await dialog.locator(`${ROWS} .slick-cell[col-id="thread"]`).allTextContents();
      return [...new Set(threads.map((t) => t.trim()))];
    })
    .toEqual(["main"]);

  await dialog.locator(ROWS).first().click({ button: "right" });
  await menuEntry(page, "Clear the query").click();
  await expect(query).toHaveValue("");
  // With no query at all, every line in the store — at least the
  // server's own.
  await expect.poll(() => lineCount(page)).toBeGreaterThanOrEqual(all);

  // The level picker writes its word into the query, where it can be
  // read back, edited or cleared like anything typed.
  const level = dialog.getByLabel("Lowest level to show");
  await expect(level).toHaveValue("trace");
  await level.selectOption("warn");
  await expect(query).toHaveValue("min_level:warn");
  await level.selectOption("trace");
  await expect(query).toHaveValue("");
});

// A tracing line carries the file and line that wrote it; the Source
// column shows them and, since the server knows which commit it came
// from (playwright.config.ts hands it one), links them to that line on
// GitHub. The link is the UI's to build: the store holds only the path
// rustc saw and the commit.
test("a line's source links to its file and line at the server's commit", async ({ page }) => {
  expect(GIT_HASH, "playwright.config.ts should have pinned DATALIB_GIT_HASH").toBeTruthy();
  const dialog = await openServerLog(page);
  const link = dialog.locator(`${ROWS} .slick-cell[col-id="source"] a`).first();
  await expect(link).toBeVisible();
  const shown = (await link.textContent()) ?? "";
  const m = /^(datalib\/backend\/.+\.rs):(\d+)$/.exec(shown.trim());
  expect(m, `source cell reads ${JSON.stringify(shown)}`).not.toBeNull();
  await expect(link).toHaveAttribute(
    "href",
    `https://github.com/imbue-ai/datalib/blob/${GIT_HASH}/${m![1]}#L${m![2]}`,
  );
  await expect(link).toHaveAttribute("target", "_blank");
});

// Grouping goes through the panel's `__fwRunLogApi.groupBy`, which
// calls the plugin's own `setDroppedGroups` — the same thing its drop
// handler calls, and how the Explore grid's spec groups too. The drag
// itself is SortableJS's native drag-and-drop, and a drag dispatched by
// hand died inside it on CI's loaded runners at more than one point
// (the header never entering the bar; the drop never ending the drag),
// through three rewrites. What is ours — the columns declared
// groupable, the placeholder, the group row's text, the toggle — is
// what this checks.
test("grouped by a column, the lines fold under group rows", async ({ page }) => {
  const dialog = await openServerLog(page);
  const bar = dialog.locator(".slick-preheader-panel .slick-dropzone");
  await expect(bar).toContainText("Drag a column here");
  await expect(dialog.locator(".slick-group-toggle-all")).toBeHidden();

  await page.evaluate(() =>
    (window as unknown as { __fwRunLogApi: { groupBy: (ids: string[]) => void } }).__fwRunLogApi.groupBy(["level"]),
  );

  // A chip for the column takes the placeholder's place in the bar…
  await expect(bar.locator(".slick-dropped-grouping")).toContainText("Level");
  await expect(bar.locator(".slick-draggable-dropzone-placeholder")).toBeHidden();
  // …and the lines sit under group rows that say what they share and
  // how many there are.
  const group = dialog.locator(".rl-grid .slick-row.slick-group").first();
  await expect(group).toBeVisible();
  await expect(group).toHaveText(/^Level: \w+ \(\d+\)$/);
  await expect(dialog.locator(".slick-group-toggle-all")).toContainText("Expand / collapse all");
});
