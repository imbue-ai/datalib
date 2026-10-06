// A credential that arrives as text: pasted into the wizard, or stored
// with `latchkey auth set` in a terminal before the app was opened.
// Slack is the example — a built-in latchkey service, so its own
// definition decides how the token is injected.
import { TNG } from "./fake_sites.mjs";
import {
  expect,
  expectGlanceable,
  expectImpersonated,
  pickTile,
  test,
  TILE,
  wizard,
  wizField,
} from "./world";

test("a pasted token is stored, then Check connection reaches the workspace", async ({
  page,
  world,
  internet,
}) => {
  await pickTile(page, TILE.slack);
  await wizard(page).getByRole("tab", { name: "Paste a key" }).click();
  const form = wizard(page).locator(".wiz-paste");
  await wizard(page).getByRole("combobox", { name: "Slack account" }).fill("enterprise");
  await form.getByLabel("Token").fill(TNG.slackToken);
  await form.getByRole("button", { name: "Store in latchkey" }).click();
  await expect(form).toContainText("Stored in latchkey.");

  // A successful paste checks the connection by itself — one request,
  // no listing.
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
    "Connected to picard in Enterprise",
  );
  expect(internet.to("slack.com", "/api/conversations.list")).toHaveLength(0);
  // latchkey's own slack service put the token on the wire.
  for (const r of internet.to("slack.com")) {
    expect(r.headers.authorization).toBe(`Bearer ${TNG.slackToken}`);
  }

  // The channel picker loads its own list, through curl-impersonate.
  await wizField(page, "Channels").locator(".wiz-load-btn").click();
  await expect(wizField(page, "Channels").locator(".wiz-load-done")).toContainText(
    "3 channels from picard in Enterprise.",
  );
  const listings = internet.to("slack.com", "/api/conversations.list");
  expect(listings).toHaveLength(2);
  for (const r of listings) expectImpersonated(r);
  // …and the config names no secret.
  await wizard(page).getByText("Review the TOML this writes").click();
  await expect(wizard(page).locator(".wiz-review pre")).not.toContainText(TNG.slackToken);
  expect(world.curlCalls().length).toBeGreaterThan(0);
});

test("a token stored on the command line beforehand just works", async ({ page, world }) => {
  world.latchkey("auth", "set", "slack", "-H", `Authorization: Bearer ${TNG.slackToken}`);

  await pickTile(page, TILE.slack);
  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("Connected to");
});

test("a wrong token fails Check connection in a sentence", async ({ page }) => {
  await pickTile(page, TILE.slack);
  await wizard(page).getByRole("tab", { name: "Paste a key" }).click();
  const form = wizard(page).locator(".wiz-paste");
  await wizard(page).getByRole("combobox", { name: "Slack account" }).fill("enterprise");
  await form.getByLabel("Token").fill("xoxp-tng-wrong");
  await form.getByRole("button", { name: "Store in latchkey" }).click();

  const failed = wizard(page).locator(".wiz-probe-failed");
  await expect(failed).toHaveAttribute("data-issue", "rejected");
  expectGlanceable(await failed.locator(".issue-headline").textContent(), "the headline");
});

/// A list that pages says how far it has got while it loads. The fake
/// holds the second page until the spec has looked, so the half-loaded
/// state is a state the spec waits for, not a race.
test("a picker's list shows its progress while it loads", async ({ page, world, internet }) => {
  world.latchkey("auth", "set", "slack", "-H", `Authorization: Bearer ${TNG.slackToken}`);
  const release = internet.hold(
    (r: { host: string; query: string }) =>
      r.host === "slack.com" && r.query.includes("cursor=page-2"),
  );
  await pickTile(page, TILE.slack);
  const channels = wizField(page, "Channels");
  await channels.locator(".wiz-load-btn").click();

  await expect(channels.locator(".wiz-load-status")).toContainText("Loading channels… 2 so far");
  await expect(channels.locator(".wiz-load-bar")).toBeVisible();
  release();
  await expect(channels.locator(".wiz-load-done")).toContainText(
    "3 channels from picard in Enterprise.",
  );
  await expect(channels.locator(".wiz-load-bar")).toHaveCount(0);
});
