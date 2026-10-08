// More than one login per service: the account box names which, a
// sign-in stores under that name, and a check runs as it. The same for
// every latchkey source.
import { TNG } from "./fake_sites.mjs";
import { expect, pickTile, subcommand, test, TILE, wizard } from "./world";
import { reviewToml, showSignIn } from "../e2e/wizard-helpers";

/// A browser login has a fresh browser to start and a fake site to
/// reach; with four workers busy that has taken ~50s.
const LOGIN = { timeout: 90_000 };

/// latchkey stores a browser login under a name it has never seen —
/// imbue-ai/latchkey#148 filed every one under the unnamed default, and
/// the backend once seeded a placeholder to get past it. Two names, two
/// credentials, and a check that runs as the one in the box.
test("Claude: two accounts by browser login, checked as the one chosen", async ({
  page,
  world,
}) => {
  await pickTile(page, TILE.claude);
  const box = wizard(page).getByRole("combobox", { name: "Claude account" });
  for (const name of ["picard", "riker"]) {
    await showSignIn(page);
    await box.fill(name);
    await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
    await expect(wizard(page).locator(".wiz-probe-ok")).toBeVisible(LOGIN);
  }
  const stored = JSON.parse(world.latchkey("auth", "list", "--offline"))["claude-ai"];
  expect(Object.keys(stored).sort()).toEqual(["picard", "riker"]);

  await showSignIn(page);
  await box.fill("riker");
  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toBeVisible();
  const runs: { args: string[] }[] = world.latchkeyRuns();
  const check = [...runs].reverse().find((r) => subcommand(r) === "curl");
  expect(check?.args.slice(0, 2)).toEqual(["--account", "riker"]);
  // The source copies the account it was checked as.
  await expect(await reviewToml(page)).toContainText('account = "riker"');
});

/// Slack had no account box at all. A pasted token goes under the name
/// in it, and a second name is a second workspace login.
test("Slack: two pasted tokens under two names, both offered back", async ({ page, world }) => {
  await pickTile(page, TILE.slack);
  const box = wizard(page).getByRole("combobox", { name: "Slack account" });
  const form = wizard(page).locator(".wiz-paste");
  for (const name of ["enterprise", "defiant"]) {
    await showSignIn(page);
    await wizard(page).getByRole("tab", { name: "Paste a key" }).click();
    await box.fill(name);
    await form.getByLabel("Token").fill(TNG.slackToken);
    await form.getByRole("button", { name: "Store in latchkey" }).click();
    // Stored, then checked at once.
    await expect(wizard(page).locator(".wiz-probe-ok")).toBeVisible();
  }
  const stored = JSON.parse(world.latchkey("auth", "list", "--offline")).slack;
  expect(Object.keys(stored).sort()).toEqual(["defiant", "enterprise"]);

  await showSignIn(page);
  await box.fill("");
  await box.click();
  await expect(
    wizard(page).getByRole("listbox", { name: "Slack account" }).getByRole("option"),
  ).toContainText(["defiant", "enterprise"]);
});

/// Who names the account a browser login adds decides what the wizard
/// sends (docs/dev/latchkey.md §"Accounts: who names them"). The backend
/// reads it off latchkey's service type; this checks that against what
/// the pinned latchkey actually does, for every service the catalog signs
/// in to with a browser, so a latchkey upgrade that changes it fails here.
test("each browser-login service names accounts the way the wizard is told", async ({
  world,
  request,
}) => {
  const naming = async (service: string) =>
    (await (await request.get(`/api/latchkey/${service}`)).json()).account_naming;

  world.installOwnGarminPlugin();
  for (const service of [
    "slack",
    "google-gmail",
    "google-calendar",
    "github",
    "fastmail",
    "garmin",
  ]) {
    expect(await naming(service), service).toBe("service");
    // latchkey refuses a name it does not hold, before any browser opens.
    const run = world.latchkeyTry("--account", "nobody-yet", "auth", "browser", service);
    expect(run.status, service).not.toBe(0);
    expect(run.stderr, service).toContain("has no credentials stored for account 'nobody-yet'");
  }

  // The two the wizard registers, as it registers them.
  world.latchkey(
    "services",
    "register",
    "claude-ai",
    "--base-api-url=https://claude.ai/",
    "--login-url=https://claude.ai/login",
    "--login-flow=cookie-capture",
    '--login-flow-params={"cookieKeys":["sessionKey"]}',
  );
  expect(await naming("claude-ai")).toBe("chosen");
  // Not registered yet: the wizard will register it.
  expect(await naming("chatgpt")).toBe("chosen");
});
