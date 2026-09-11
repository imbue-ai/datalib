// Slack: the Connection block fills the channel picker and, once DMs
// are on, the people picker — through the same probe and the same
// grid the email and Claude forms use.
import { test, expect, type Page } from "@playwright/test";

const wizard = (page: Page) => page.getByRole("dialog");
const field = (page: Page, caption: string) =>
  wizard(page)
    .locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-input`)
    .first();
const toggle = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-bool`);
const picker = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .pick-grid`);
const rows = (page: Page, caption: string) => picker(page, caption).locator(".ag-row");
const tick = (page: Page, caption: string, id: string) =>
  picker(page, caption)
    .locator(`.ag-row[row-id="${id}"] .ag-selection-checkbox input`)
    .first()
    .click();

const SLACK_SERVICE = {
  service: "slack",
  auth_options: ["browser", "set"],
  accounts: [{ account: "", credential_type: "token", credential_status: "valid" }],
  error: null,
};

/// What `datalib-step probe slack` prints for a small workspace: every
/// channel the account can see, tagged where it matters, and one row
/// per person on the far end of a DM.
const SLACK_PROBE = {
  mode: "api",
  account: { id: "U_PICARD", address: null, display_name: "picard in Enterprise", message_estimate: null },
  items: [
    { path: "bridge", kind: "channel", title: null, role: null, messages: null, members: 12, updated_at: null },
    { path: "engineering", kind: "channel", title: null, role: "private", messages: null, members: 4, updated_at: null },
    { path: "ten-forward", kind: "channel", title: null, role: "not a member", messages: null, members: 40, updated_at: null },
    { path: "U_RIKER", kind: "person", title: "William Riker", role: "@riker", messages: null, members: null, updated_at: null },
    { path: "U_DATA", kind: "person", title: "Data", role: "@data", messages: null, members: null, updated_at: null },
  ],
  notes: [],
};

let lastProbeRequest: { type?: string; params?: Record<string, unknown> } = {};

async function stubBackend(page: Page) {
  await page.route("**/api/latchkey/slack", (route) => route.fulfill({ json: SLACK_SERVICE }));
  await page.route("**/api/probe", (route) => {
    lastProbeRequest = route.request().postDataJSON();
    return route.fulfill({ json: SLACK_PROBE });
  });
}

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

async function pickSlack(page: Page) {
  await page.getByRole("button", { name: "+ Data Source" }).click();
  await page.getByRole("searchbox").fill("slack");
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror channels and DMs from one Slack workspace." })
    .click();
}

let original = "";

test.beforeEach(async ({ page }) => {
  lastProbeRequest = {};
  await stubBackend(page);
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});

test("a probe fills the channel picker, and ticking rows writes `channels`", async ({
  page,
}) => {
  await pickSlack(page);

  // Nothing to pick from until the workspace has been asked.
  await expect(picker(page, "Channels")).toHaveCount(0);
  await wizard(page).getByRole("button", { name: "Test connection" }).click();

  // A Slack account has no address, so the line names the handle and
  // the workspace, and counts both kinds of thing that came back.
  await expect(wizard(page).locator(".wiz-probe-note")).toContainText(
    "Reached picard in Enterprise — 3 channels, 2 people.",
  );
  // The probe authenticates with what Save would write: the `api`
  // table that selects the live method, with the form's defaults.
  expect(lastProbeRequest.type).toBe("slack");
  expect(lastProbeRequest.params).toMatchObject({ api: { media: true, dms: false } });

  // Channels read with their `#`, and a tag says why one might not be
  // what you expect; membership doesn't gate the list, since naming a
  // channel mirrors it either way.
  await expect(rows(page, "Channels")).toHaveText([
    /#bridge.*12/,
    /#engineering.*private.*4/,
    /#ten-forward.*not a member.*40/,
  ]);
  // The people picker belongs to a field that only exists once DMs
  // are on, so it isn't on the page yet.
  await expect(picker(page, "Only DMs with these people")).toHaveCount(0);

  await tick(page, "Channels", "engineering");
  await tick(page, "Channels", "bridge");
  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  // The bare name, never the `#` the grid shows.
  await expect(toml).toContainText('channels = ["engineering", "bridge"]');
});

test("turning DMs on reveals a people picker filled from the same probe", async ({ page }) => {
  await pickSlack(page);
  await wizard(page).getByRole("button", { name: "Test connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-note")).toContainText("Reached");

  // One probe, both pickers: no second "Test connection" after the
  // toggle.
  await toggle(page, "Download direct messages").check();
  await expect(rows(page, "Only DMs with these people")).toHaveText([
    /William Riker.*@riker/,
    /Data.*@data/,
  ]);

  await tick(page, "Only DMs with these people", "U_RIKER");
  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  // The user id: the one spelling of a person that can't collide or
  // be renamed out from under the config.
  await expect(toml).toContainText('dm_users = ["U_RIKER"]');
  await expect(toml).toContainText("dms = true");
});

test("a typed name is checked the way the downloader reads it", async ({ page }) => {
  await pickSlack(page);
  // `#bridge` is fine (the downloader strips the `#`); `bridg` is not
  // a channel.
  await field(page, "Channels").fill("#bridge, bridg");
  await toggle(page, "Download direct messages").check();
  // A handle with its `@`, a real name in the wrong case, and a
  // stranger: `dm_users` resolves the first two, and only the third
  // would mirror nothing.
  await field(page, "Only DMs with these people").fill("@riker, william riker, wesley");
  await wizard(page).getByRole("button", { name: "Test connection" }).click();

  await expect(wizard(page).getByText(/Not on this account: bridg\./)).toBeVisible();
  await expect(wizard(page).getByText(/Not on this account: wesley\./)).toBeVisible();
});
