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

/// Naming an account is hidden *for Claude*, because latchkey cannot
/// honour it here: a
/// browser login accepts `--account`, reports success, and files the
/// credential under the unnamed default anyway
/// (imbue-ai/latchkey#148). Offering a picker whose value the login
/// ignores is how a config comes to name an account whose credential
/// lives somewhere else — a sync that fails later, far from the cause.
///
/// Scoped to services we register with a cookie capture, which is where
/// the login has no identity to learn. `wizard-email.spec.ts` holds the
/// other side: Fastmail and Gmail are built-in OAuth, their logins do
/// file under the address signed in with, and their picker stays.
test("no account picker, and the login runs as latchkey's default", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);

  await expect(
    wizard(page).locator('.wiz-field:has(> .wiz-label:text-is("Claude account"))'),
  ).toHaveCount(0);
  await expect(wizard(page).locator("select.wiz-accountpick")).toHaveCount(0);

  let connectBody: { account?: string } | null = null;
  await page.route("**/api/latchkey/claude-ai/connect", (route) => {
    connectBody = route.request().postDataJSON();
    return route.fulfill({ json: { id: "a1", status: "running", output: "" } });
  });
  await wizard(page).getByRole("button", { name: "Latchkey auth" }).click();

  // Empty means "latchkey's own default", which is addressed by sending
  // no `--account` at all.
  await expect.poll(() => connectBody?.account).toBe("");
});

/// The other half of "always the default": with nothing to type an
/// account into, nothing writes one, so the step names no identity and
/// latchkey uses its own. `source_steps.test.ts` covers the converse —
/// the params plumbing still carries an account when one is in the
/// values, which is what makes this hidden rather than removed.
test("a new source writes no account at all", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);
  await wizard(page).getByText("Review the TOML this writes").click();
  await expect(wizard(page).locator(".wiz-review pre")).not.toContainText("latchkey_settings");
});
