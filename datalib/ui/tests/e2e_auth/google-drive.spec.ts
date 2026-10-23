// Google Drive's download reaches four latchkey services, so its one
// "Sign in with browser" signs in to each in turn as one account, and a
// missing OAuth client is pasted once for all four. Google is not faked
// past its consent screen, which turns every sign-in down
// (fake_sites.mjs), so these stop at the first login that reaches it.
import { TNG } from "./fake_sites.mjs";
import { expect, pickTile, subcommand, test, TILE, wizard } from "./world";

const LOGIN = { timeout: 90_000 };
const SERVICES = ["google-drive", "google-docs", "google-sheets", "google-slides"];
const PICARD = "picard@enterprise.test";
const CLIENT = JSON.stringify({
  clientId: TNG.googleClientId,
  clientSecret: TNG.googleClientSecret,
});

type Run = { args: string[] };
const runsSince = (runs: Run[], from: number, sub: string) =>
  runs.slice(from).filter((r) => subcommand(r) === sub);

test("a pasted client is stored for all four services, and the sign-in starts at Drive", async ({
  page,
  world,
  internet,
}) => {
  await pickTile(page, TILE.drive);
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect(wizard(page).locator(".wiz-connect-failed")).toHaveAttribute(
    "data-issue",
    "no_oauth_client",
    LOGIN,
  );
  const panel = wizard(page).locator(".wiz-oauth-client");
  // Creating one would make a Cloud project per service.
  await expect(panel.getByRole("button", { name: "Create one for me" })).toHaveCount(0);

  const from = world.latchkeyRuns().length;
  await panel.getByRole("textbox").fill(
    JSON.stringify({
      installed: {
        client_id: TNG.googleClientId,
        client_secret: TNG.googleClientSecret,
        redirect_uris: ["http://localhost"],
      },
    }),
  );
  await panel.getByRole("button", { name: "Save client and sign in" }).click();

  await expect(wizard(page).locator(".wiz-connect-failed")).toContainText("access_denied", LOGIN);
  const runs: Run[] = world.latchkeyRuns();
  expect(runsSince(runs, from, "auth prepare").map((r) => r.args.slice(-2))).toEqual(
    SERVICES.map((s) => [s, CLIENT]),
  );
  // Drive's consent turned it down, so the three after it never ran.
  expect(runsSince(runs, from, "auth browser").map((r) => r.args.at(-1))).toEqual(["google-drive"]);
  const consent = internet.to("accounts.google.com", "/o/oauth2/v2/auth");
  expect(new URLSearchParams(consent.at(-1)?.query).get("client_id")).toBe(TNG.googleClientId);
  await expect(panel).toBeHidden();
});

/// A sign-in that stopped after Drive is finished by the three logins
/// left. Each of them names its own account, so none is sent one it does
/// not hold yet: latchkey would refuse it before any browser opened.
test("a half-done sign-in goes on from the first service that lacks the account", async ({
  page,
  world,
}) => {
  for (const s of SERVICES) world.latchkey("auth", "prepare", s, CLIENT);
  world.latchkey(
    "--account",
    PICARD,
    "auth",
    "set",
    "google-drive",
    "-H",
    "Authorization: Bearer ya29.tng-picard",
  );

  await pickTile(page, TILE.drive);
  await wizard(page).getByRole("combobox", { name: "Google account" }).fill(PICARD);
  await expect(wizard(page).locator(".wiz-partly-signed-in")).toHaveText(
    "Signed in to Google Drive; not yet to Google Docs, Google Sheets and Google Slides.",
  );

  const from = world.latchkeyRuns().length;
  await wizard(page)
    .getByRole("button", { name: `Finish signing in as ${PICARD}` })
    .click();
  await expect(wizard(page).locator(".wiz-connect-failed")).toContainText("access_denied", LOGIN);
  const browser = runsSince(world.latchkeyRuns(), from, "auth browser");
  expect(browser.map((r) => r.args.at(-1))).toEqual(["google-docs"]);
  expect(browser[0].args).not.toContain("--account");
});
