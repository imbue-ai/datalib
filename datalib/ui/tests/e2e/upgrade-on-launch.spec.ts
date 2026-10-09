// What a launch on an upgraded root shows (docs/dev/plans/upgrade_on_launch.md):
// a blocking screen while every step is asked to migrate, then the offer to
// re-render. The server's side — which steps are asked, the pass before
// any sync, what the offer lists — is the Rust tests'; here `/api/config`'s
// `upgrade` is set to each state in turn and the re-render request is
// caught before it reaches the shared root.

import { test, expect, type Page, type Request } from "@playwright/test";

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
    "unified_index/grid_index",
  ],
};

test("the migrate pass blocks the app and says how each source is going", async ({ page }) => {
  await launchWith(page, MIGRATING);
  await expect(
    page.getByRole("heading", { name: "Updating your data for this version of datalib" }),
  ).toBeVisible();
  const rows = page.locator(".upgrading .stores li");
  // One row per source, however many steps it has.
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(0)).toContainText("enterprise-mail");
  await expect(rows.nth(0)).toContainText("Done");
  await expect(rows.nth(1)).toContainText("Updating");
  await expect(rows.nth(2)).toContainText("Waiting");
  // Nothing behind it can be used while it runs.
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toHaveCount(0);
});

test("after the pass, a yes re-runs what asked for it, as the upgrade", async ({ page }) => {
  const asked: Request[] = [];
  await page.route("**/api/requests", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    asked.push(route.request());
    await route.fulfill({ json: [] });
  });
  await launchWith(page, MIGRATED);

  const dialog = page.getByRole("dialog", { name: "Your rendered documents are out of date" });
  await expect(dialog).toBeVisible();
  const sources = dialog.locator("ul.sources:not(.failed) li");
  await expect(sources).toHaveText(["enterprise-mail", "ten-forward", "Search index"]);
  await expect(dialog.locator("ul.failed li")).toContainText("holodeck-slack");
  await expect(dialog.locator("ul.failed li")).toContainText("no rung carries it");

  await dialog.getByRole("button", { name: "Re-render now" }).click();
  await expect(dialog).toHaveCount(0);
  await expect.poll(() => asked.length).toBe(1);
  expect(asked[0].postDataJSON()).toEqual({ roots: MIGRATED.rerender, by: "upgrade" });
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toBeVisible();
});

test("not now leaves the steps alone until the next launch", async ({ page }) => {
  let posted = 0;
  await page.route("**/api/requests", async (route) => {
    if (route.request().method() === "POST") posted += 1;
    await route.fallback();
  });
  await launchWith(page, MIGRATED);

  const dialog = page.getByRole("dialog", { name: "Your rendered documents are out of date" });
  await dialog.getByRole("button", { name: "Not now" }).click();
  await expect(dialog).toHaveCount(0);
  expect(posted).toBe(0);

  await page.reload();
  await expect(dialog).toBeVisible();
});

test("a step the pass could not ask opens no dialog on its own", async ({ page }) => {
  await launchWith(page, {
    migrating: false,
    settled: true,
    steps: [{ step: "enterprise-mail/ingest", state: "failed", error: "no datalib-step" }],
    rerender: [],
  });
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toBeVisible();
  await expect(
    page.getByRole("dialog", { name: "Your rendered documents are out of date" }),
  ).toHaveCount(0);
});
