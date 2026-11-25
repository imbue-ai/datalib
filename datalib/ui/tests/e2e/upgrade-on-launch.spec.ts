// What a launch on an upgraded root shows (dag/README.md §"Upgrading a root"):
// the upgrade screen in front of the library while every step is asked to
// prepare and while the steps it found in another shape re-run, which the
// server starts by itself; then the library, with the pill for a source the
// pass could not update. The server's side — which steps are asked, the
// pass before any sync, the re-runs it opens — is the Rust tests'; here
// `/api/config`'s `upgrade` is set to each state in turn.

import { test, expect, type Page } from "@playwright/test";

type Upgrade = {
  migrating: boolean;
  settled: boolean;
  steps: { step: string; state: string; error: string | null }[];
  rerender: string[];
};

async function launchWith(page: Page, upgrade: Upgrade) {
  await page.route("**/api/config", async (route) => {
    if (route.request().method() !== "GET") return route.fallback();
    const real = await route.fetch();
    const json = { ...(await real.json()), upgrade };
    await route.fulfill({ response: real, json });
  });
  await page.goto("/");
}

const MIGRATING: Upgrade = {
  migrating: true,
  settled: false,
  steps: [
    { step: "enterprise-mail/ingest", state: "done", error: null },
    { step: "enterprise-mail/render_markdown", state: "done", error: null },
    { step: "holodeck-slack/ingest", state: "done", error: null },
    { step: "holodeck-slack/render_markdown", state: "running", error: null },
    { step: "ten-forward/ingest", state: "waiting", error: null },
  ],
  rerender: [],
};

const MIGRATED: Upgrade = {
  migrating: false,
  settled: true,
  steps: [
    { step: "enterprise-mail/ingest", state: "done", error: null },
    {
      step: "holodeck-slack/ingest",
      state: "failed",
      error: "messages.body was renamed and no rung carries it",
    },
  ],
  rerender: [
    "enterprise-mail/render_markdown",
    "ten-forward/render_markdown",
    "unified_grid_and_search_index/unified_grid_and_search_index",
  ],
};

test("the pass puts the upgrade screen in front of the library, saying how each source is going", async ({
  page,
}) => {
  await launchWith(page, MIGRATING);
  const screen = page.getByTestId("upgrading");
  await expect(
    screen.getByRole("heading", { name: "Updating your library for this version" }),
  ).toBeVisible();
  // Nothing reads a store in another shape, so there is nothing to search yet.
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toHaveCount(0);
  // One row per source, however many steps it has; a source is done
  // when every step of it is.
  const rows = screen.locator(".rows li");
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(0)).toContainText("enterprise-mail");
  await expect(rows.nth(0)).toContainText("Done");
  await expect(rows.nth(1)).toContainText("Updating");
  await expect(rows.nth(2)).toContainText("Waiting");
});

test("after the pass, the screen waits on the re-runs the server started, asking nothing", async ({
  page,
}) => {
  let posted = 0;
  await page.route("**/api/requests", async (route) => {
    if (route.request().method() === "POST") posted += 1;
    await route.fallback();
  });
  await launchWith(page, MIGRATED);
  const screen = page.getByTestId("upgrading");
  await expect(screen.locator(".rows li")).toHaveText([
    /enterprise-mail.*Re-running/,
    /ten-forward.*Re-running/,
    /Search index.*Re-running/,
  ]);
  await expect(screen).toContainText("Nothing is downloaded");
  expect(posted).toBe(0);
});

test("Open Sources lifts the screen for a source whose re-run is stuck", async ({ page }) => {
  await launchWith(page, MIGRATED);
  await page.getByTestId("upgrading").getByRole("button", { name: "Open Sources" }).click();
  await expect(page.getByTestId("upgrading")).toHaveCount(0);
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toBeVisible();
  // The source the pass could not update is the pill's to report.
  await expect(page.getByRole("button", { name: "1 source could not be updated" })).toBeVisible();
});

test("a step the pass could not ask turns the pill red, not a dialog, until dismissed", async ({
  page,
}) => {
  await launchWith(page, {
    migrating: false,
    settled: true,
    steps: [
      { step: "enterprise-mail/ingest", state: "failed", error: "no datalib-step" },
      { step: "holodeck-slack/ingest", state: "done", error: null },
    ],
    rerender: [],
  });
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toBeVisible();
  await expect(page.getByTestId("upgrading")).toHaveCount(0);

  const pill = page.getByRole("button", { name: "1 source could not be updated" });
  await pill.click();
  const menu = page.getByRole("dialog", { name: "1 source could not be updated" });
  await expect(menu).toContainText("1 of 2 sources are up to date");
  // Only what failed is listed, with its reason.
  await expect(menu.locator(".pass-list li")).toHaveCount(1);
  await expect(menu.locator(".pass-list li")).toContainText("enterprise-mail");
  await expect(menu.locator(".pass-list li")).toContainText("no datalib-step");
  await menu.getByRole("button", { name: "Dismiss" }).click();
  await expect(pill).toHaveCount(0);
});
