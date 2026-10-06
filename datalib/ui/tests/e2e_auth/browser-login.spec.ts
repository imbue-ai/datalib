// "Sign in with browser" end to end: the backend registers the service
// latchkey does not ship, finds a browser, and runs `auth browser`; a
// real (headless) Chromium loads the fake site's login page, latchkey
// captures the credential, and Check connection then uses it.
//
// The prep steps are load-bearing here, not set up by the harness:
// latchkey's own `ensure-browser` picks the browser (fake_node.mjs only
// makes what it picked headless and points it at the fake internet),
// and `services register` is what makes `claude-ai` a name latchkey
// knows. Drop either and the login fails.
import { TNG } from "./fake_sites.mjs";
import {
  expect,
  expectGlanceable,
  expectImpersonated,
  pickTile,
  subcommand,
  test,
  TILE,
  wizard,
  wizField,
} from "./world";

const LOGIN_PREP = ["services register", "ensure-browser", "auth browser"];

/// A login is three latchkey runs and a browser start; with four workers
/// busy it has taken ~50s on a laptop.
const LOGIN = { timeout: 90_000 };

test("Claude: a cookie-capture login, then Check connection and Load", async ({
  page,
  world,
  internet,
}) => {
  await pickTile(page, TILE.claude);
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect(wizard(page)).toContainText("Connected.", LOGIN);

  const runs = world.latchkeyRuns().map(subcommand);
  expect(runs.filter((r) => LOGIN_PREP.includes(r))).toEqual(LOGIN_PREP);
  expect(world.browserFound(), "latchkey's ensure-browser should have found one").toBeTruthy();
  expect(internet.to("claude.ai", "/login")).not.toHaveLength(0);

  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("picard@enterprise.test");
  const api = internet.to("claude.ai").filter((r) => r.path.startsWith("/api/"));
  expect(api).not.toHaveLength(0);
  for (const r of api) {
    expect(r.headers.cookie).toContain(`sessionKey=${TNG.claudeSessionKey}`);
    expectImpersonated(r);
  }

  const conversations = wizField(page, "Only these conversations");
  await conversations.locator(".wiz-load-btn").click();
  await expect(conversations.locator(".wiz-load-done")).toContainText(
    "2 conversations from picard@enterprise.test.",
  );
});

test("ChatGPT: a token-capture login, then Check connection and Load", async ({
  page,
  internet,
}) => {
  await pickTile(page, TILE.chatgpt);
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect(wizard(page)).toContainText("Connected.", LOGIN);

  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("picard@enterprise.test");
  const api = internet.to("chatgpt.com").filter((r) => r.path.startsWith("/backend-api/"));
  expect(api).not.toHaveLength(0);
  for (const r of api) {
    expect(r.headers.authorization).toBe(`Bearer ${TNG.chatgptAccessToken}`);
    expectImpersonated(r);
  }

  const conversations = wizField(page, "Only these conversations");
  await conversations.locator(".wiz-load-btn").click();
  await expect(conversations.locator(".wiz-load-done")).toContainText(
    "1 conversation from picard@enterprise.test.",
  );
});

test.describe("with no browser on the machine", () => {
  test.use({ worldOptions: { browser: "download" } });

  /// The person pressed "Sign in with browser"; fetching one is what
  /// they asked for, so the wizard does it and says so while it runs.
  test("the wizard fetches one and says so", async ({ page, world }) => {
    await pickTile(page, TILE.claude);
    await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
    await expect(wizard(page).locator(".wiz-connect-status")).toContainText(
      "Getting a browser for the sign-in",
    );
    // The button says what it is waiting on, too — not "the browser",
    // which does not exist yet.
    await expect(wizard(page).getByRole("button", { name: "Getting a browser…" })).toBeDisabled();
    world.releaseDownload();
    await expect(wizard(page)).toContainText("Connected.", LOGIN);
    const looks = world
      .latchkeyRuns()
      .filter((r) => subcommand(r) === "ensure-browser")
      .map((r) => r.args.at(-1));
    expect(looks).toEqual([
      "existing-config,system-browser,existing-playwright-browser",
      "download-playwright-browser",
    ]);
  });
});

test.describe("with no browser, and none to be had", () => {
  test.use({ worldOptions: { browser: "none" } });

  test("the login says so in a sentence", async ({ page }) => {
    await pickTile(page, TILE.claude);
    await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
    const failed = wizard(page).locator(".wiz-connect-failed");
    await expect(failed).toHaveAttribute("data-issue", "no_browser", LOGIN);
    const headline = await failed.locator(".issue-headline").textContent();
    expectGlanceable(headline, "the login failure");
    expect(headline).not.toContain("ensure-browser");
  });
});
