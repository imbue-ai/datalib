// The Connection block on a latchkey service that has no browser login.
//
// A browser login belongs to the *service* and is fixed when it is
// registered; latchkey refuses to re-register a name it already holds.
// So on a machine where `claude-ai` was set up by hand there is no
// non-destructive way to add one — and the wizard's job is to say so
// with the commands, not to take the service apart on a click.
//
// Read-only: nothing here saves, so it runs against the shared fixture
// root rather than a sandbox of its own.
import { test, expect, type Page } from "@playwright/test";

const wizard = (page: Page) => page.getByRole("dialog");

/// `latchkey services info claude-ai` on a machine that registered it
/// the way the docs used to say: generic, `set` only, one credential
/// whose validity latchkey cannot check.
/// An account latchkey already holds, so `--account` can name it.
const STORED_ACCOUNT = "picard@enterprise.gov";

const SET_ONLY = {
  service: "claude-ai",
  auth_options: ["set"],
  accounts: [
    { account: "", credential_type: "rawCurl", credential_status: "unknown" },
    { account: STORED_ACCOUNT, credential_type: "rawCurl", credential_status: "unknown" },
  ],
  registered: true,
  cli: "/opt/datalib/bin/latchkey",
  error: null,
};

/// The same service registered with a cookie-capture login, which is
/// what the commands below produce.
const WITH_BROWSER = { ...SET_ONLY, auth_options: ["browser", "set"] };

async function openClaude(page: Page, service: object) {
  await page.route("**/api/latchkey/claude-ai", (route) => route.fulfill({ json: service }));
  await page.goto("/sources2");
  await page.getByRole("button", { name: "+ Data Source" }).click();
  // By blurb: "Claude" alone also matches the Claude export tile.
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Mirror your claude.ai conversations" })
    .click();
}

test("the auth button is offered even when the service can't do it yet", async ({ page }) => {
  await openClaude(page, SET_ONLY);

  // The regression this guards: the button was hidden entirely on a
  // set-only service, which reads as the feature being missing.
  const auth = wizard(page).getByRole("button", { name: "Latchkey auth" });
  await expect(auth).toBeVisible();

  // Clicking it must not touch latchkey. A conversion destroys every
  // credential stored under the service, so the click explains and
  // stops.
  let connectCalls = 0;
  await page.route("**/api/latchkey/claude-ai/connect", (route) => {
    connectCalls += 1;
    return route.fulfill({ json: { id: "x", status: "running", output: "" } });
  });
  await auth.click();

  const convert = wizard(page).locator(".wiz-convert");
  await expect(convert).toContainText("deletes every credential stored under");
  // The exact commands, naming *this* machine's latchkey rather than a
  // bare `latchkey` that may not be on anyone's PATH.
  const commands = convert.locator("pre");
  await expect(commands).toContainText("/opt/datalib/bin/latchkey auth clear claude-ai --all");
  await expect(commands).toContainText("services deregister claude-ai");
  await expect(commands).toContainText("--login-flow=cookie-capture");
  await expect(commands).toContainText('--login-flow-params=\'{"cookieKeys":["sessionKey"]}\'');
  expect(connectCalls).toBe(0);

  // The paste route is named too — it needs no conversion and is what
  // this service already does.
  await expect(wizard(page)).toContainText("latchkey auth set claude-ai");
});

/// The account box, which is a dropdown *and* a free-text field.
const accountBox = (page: Page) =>
  wizard(page).locator('.wiz-field:has(> .wiz-label:text-is("Claude account")) input.wiz-input');

test("once the service has a browser login, the button just runs it", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);

  let connectBody: { account?: string } | null = null;
  await page.route("**/api/latchkey/claude-ai/connect", (route) => {
    connectBody = route.request().postDataJSON();
    return route.fulfill({ json: { id: "a1", status: "running", output: "" } });
  });

  // An account latchkey already holds is passed through: `--account` is
  // what decides which identity the login refreshes.
  await accountBox(page).fill(STORED_ACCOUNT);
  await wizard(page).getByRole("button", { name: "Latchkey auth" }).click();

  await expect.poll(() => connectBody?.account).toBe(STORED_ACCOUNT);
  await expect(wizard(page).locator(".wiz-convert")).toHaveCount(0);
});

/// The bug this guards, seen on a real machine: typing a *new* account
/// name and pressing the button produced "No credentials stored for
/// account 'thad_test_2' of service 'claude-ai'". latchkey's
/// `--account` selects a credential to refresh and cannot create one,
/// and dropping the flag would be worse — the login would succeed and
/// store under latchkey's unnamed default while the config being
/// written names `thad_test_2`, which resolves to nothing at sync time.
test("a name latchkey doesn't hold blocks the button and says what to run", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);

  let connectCalls = 0;
  await page.route("**/api/latchkey/claude-ai/connect", (route) => {
    connectCalls += 1;
    return route.fulfill({ json: { id: "a1", status: "running", output: "" } });
  });

  await accountBox(page).fill("thad_test_2");
  const auth = wizard(page).getByRole("button", { name: "Latchkey auth" });
  await expect(auth).toBeDisabled();
  await expect(wizard(page).locator(".wiz-newaccount")).toContainText(
    "--account thad_test_2 auth set claude-ai",
  );
  expect(connectCalls).toBe(0);

  // Clearing the box is the other way through — latchkey's own default
  // account needs no `--account` and no seeding.
  await accountBox(page).fill("");
  await expect(auth).toBeEnabled();
});
