// A Google sign-in on a latchkey store with no OAuth client for the
// service: the real latchkey refuses `auth browser` until one is
// prepared, and the wizard offers both ways to give it one. Google itself
// is not faked past its consent screen, which turns the sign-in down
// (fake_sites.mjs), so these stop where the pasted client reaches Google.
import { TNG } from "./fake_sites.mjs";
import { expect, subcommand, test, TILE, wizard } from "./world";

const LOGIN = { timeout: 90_000 };

const DOWNLOADED_CLIENT = JSON.stringify({
  installed: {
    client_id: TNG.googleClientId,
    project_id: "latchkey-gmail-tng",
    auth_uri: "https://accounts.google.com/o/oauth2/auth",
    token_uri: "https://oauth2.googleapis.com/token",
    client_secret: TNG.googleClientSecret,
    redirect_uris: ["http://localhost"],
  },
});

test.beforeEach(async ({ page }) => {
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  await wizard(page).locator(".wiz-tile", { hasText: TILE.gmail }).click();
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect(wizard(page).locator(".wiz-connect-failed")).toHaveAttribute(
    "data-issue",
    "no_oauth_client",
    LOGIN,
  );
});

test("a pasted client is stored for the service and the sign-in goes out with it", async ({
  page,
  world,
  internet,
}) => {
  const panel = wizard(page).locator(".wiz-oauth-client");
  await panel.getByRole("textbox").fill(DOWNLOADED_CLIENT);
  await panel.getByRole("button", { name: "Save client and sign in" }).click();

  // Google turned it down, which is as far as the fake goes.
  await expect(wizard(page).locator(".wiz-connect-failed")).toContainText("access_denied", LOGIN);
  const prepare = world.latchkeyRuns().find((r) => subcommand(r) === "auth prepare");
  expect(prepare?.args.slice(-2)).toEqual([
    "google-gmail",
    JSON.stringify({ clientId: TNG.googleClientId, clientSecret: TNG.googleClientSecret }),
  ]);
  const consent = internet.to("accounts.google.com", "/o/oauth2/v2/auth");
  expect(consent).toHaveLength(1);
  expect(new URLSearchParams(consent[0].query).get("client_id")).toBe(TNG.googleClientId);
  // The client is in place, so the panel offering one has gone.
  await expect(panel).toBeHidden();
});

test("a client with no secret is refused before latchkey sees it", async ({ page, world }) => {
  const panel = wizard(page).locator(".wiz-oauth-client");
  await panel.getByRole("textbox").fill(JSON.stringify({ installed: { client_id: "x" } }));
  await panel.getByRole("button", { name: "Save client and sign in" }).click();
  await expect(panel.locator(".issue")).toContainText("client_secret");
  expect(world.latchkeyRuns().map(subcommand)).not.toContain("auth prepare");
});

test("Create one for me runs browser-prepare on the Cloud console before the sign-in", async ({
  page,
  world,
  internet,
}) => {
  const panel = wizard(page).locator(".wiz-oauth-client");
  await expect(panel).toContainText("Latchkey-gmail");
  await panel.getByRole("button", { name: "Create one for me" }).click();
  await expect(wizard(page).locator(".wiz-connect-status")).toContainText(
    "Creating the OAuth client",
  );
  await expect(wizard(page).getByRole("button", { name: "Creating the client…" })).toBeDisabled();
  // The fake console wants 2-Step Verification, as Google's does of an
  // account without it, and latchkey says so.
  await expect(wizard(page).locator(".wiz-connect-failed")).toContainText(
    "Enable multi-factor authentication",
    LOGIN,
  );
  expect(internet.to("console.cloud.google.com")).not.toHaveLength(0);
  const runs = world.latchkeyRuns().map(subcommand);
  expect(runs.filter((r) => r.startsWith("auth "))).toEqual([
    "auth browser",
    "auth browser-prepare",
  ]);
  // A failed creation leaves both ways on offer.
  await expect(panel.getByRole("button", { name: "Create one for me" })).toBeEnabled();
});
