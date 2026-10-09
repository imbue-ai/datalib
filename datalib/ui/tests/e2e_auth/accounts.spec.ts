// More than one login per service: the account box names which, a
// sign-in stores under that name, and a check runs as it. The same for
// every latchkey source.
import { TNG } from "./fake_sites.mjs";
import { expect, pickTile, subcommand, test, TILE, wizard } from "./world";
import { reviewToml, showSignIn } from "../e2e/wizard-helpers";

/// A browser login has a fresh browser to start and a fake site to
/// reach; with four workers busy that has taken ~50s.
const LOGIN = { timeout: 90_000 };

/// The two services whose login cannot tell who signed in, so latchkey
/// files it under the name in the box.
const NAMED_BY_THE_BOX = [
  {
    tile: TILE.claude,
    label: "Claude account",
    service: "claude-ai",
    host: "claude.ai",
    login: "/login",
  },
  {
    tile: TILE.chatgpt,
    label: "ChatGPT account",
    service: "chatgpt",
    host: "chatgpt.com",
    login: "/auth/login",
  },
];

for (const { tile, label, service, host, login } of NAMED_BY_THE_BOX) {
  /// latchkey's saved browser session is one for every service and
  /// account, still signed in as whoever used it last. A second account
  /// started from it got the first person's credential under the second
  /// name, and every check of it reached the first person.
  test(`${label}: a second account is a second person`, async ({ page, world, internet }) => {
    await pickTile(page, tile);
    const box = wizard(page).getByRole("combobox", { name: label });
    for (const name of ["picard", "riker"]) {
      internet.signInAs(name);
      await showSignIn(page);
      await box.fill(name);
      await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
      await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
        `${name}@enterprise.test`,
        LOGIN,
      );
    }
    const stored = JSON.parse(world.latchkey("auth", "list", "--offline"))[service];
    expect(Object.keys(stored).sort()).toEqual(["picard", "riker"]);

    for (const name of ["picard", "riker"]) {
      await showSignIn(page);
      await box.fill(name);
      await wizard(page).getByRole("button", { name: "Check connection" }).click();
      await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(`${name}@enterprise.test`);
      const runs: { args: string[] }[] = world.latchkeyRuns();
      const check = [...runs].reverse().find((r) => subcommand(r) === "curl");
      expect(check?.args.slice(0, 2)).toEqual(["--account", name]);
    }
    // The source copies the account it was checked as.
    await expect(await reviewToml(page)).toContainText('account = "riker"');
  });

  /// Signing in again to the one account latchkey holds starts from the
  /// saved session, which the site sends straight on with no new cookie;
  /// latchkey takes the cookie from the browser (imbue-ai/latchkey#150).
  /// The site would sign in Riker if asked, so a fresh sign-in shows.
  test(`${label}: signing in again keeps the saved session`, async ({ page, world, internet }) => {
    await pickTile(page, tile);
    const signIn = wizard(page).getByRole("button", { name: "Sign in with browser" });
    await signIn.click();
    await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
      "picard@enterprise.test",
      LOGIN,
    );

    // A login is checked at once, so one more `curl` is the second
    // login done; the first one's result is still on screen until then.
    const checks = () =>
      world.latchkeyRuns().filter((r: { args: string[] }) => subcommand(r) === "curl").length;
    const checksBefore = checks();
    internet.signInAs("riker");
    await showSignIn(page);
    await signIn.click();
    await expect.poll(checks, LOGIN).toBeGreaterThan(checksBefore);
    await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("picard@enterprise.test");
    expect(
      internet.to(host, login).at(-1)?.headers.cookie,
      "the second login should arrive signed in",
    ).toBeTruthy();
  });
}

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
    await form.getByRole("button", { name: "Save sign-in" }).click();
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
