// The account row on a latchkey service that has no browser login.
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
import { reviewToml } from "./wizard-helpers";
import { probeDone, probeFailed } from "./probe-stub";

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
  gateway: null,
  error: null,
  account_naming: "chosen",
};

/// The same service registered with a cookie-capture login, which is
/// what the commands below produce.
const WITH_BROWSER = { ...SET_ONLY, auth_options: ["browser", "set"] };

async function openClaude(page: Page, service: object) {
  await page.route("**/api/latchkey/claude-ai", (route) => route.fulfill({ json: service }));
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  // By blurb: "Claude" alone also matches the Claude export tile.
  await wizard(page).locator(".wiz-tile", { hasText: "Copy your claude.ai conversations" }).click();
}

test("the auth button is offered even when the service can't do it yet", async ({ page }) => {
  await openClaude(page, SET_ONLY);

  // The regression this guards: the button was hidden entirely on a
  // set-only service, which reads as the feature being missing.
  const auth = wizard(page).getByRole("button", { name: "Sign in with browser" });
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
  // this service already does — and its tab gives the terminal command.
  await expect(convert).toContainText("Paste a key");
  await wizard(page).getByRole("tab", { name: "Paste a key" }).click();
  await expect(wizard(page)).toContainText("latchkey auth set claude-ai");
});

/// Claude has an account box like every other latchkey source: a
/// browser login stores under the name in it (imbue-ai/latchkey#148,
/// which filed every login under the unnamed default, is fixed), so a
/// second claude.ai account is a second name.
test("the login runs as the account named in the box", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);

  const box = wizard(page).getByRole("combobox", { name: "Claude account" });
  await expect(box).toBeVisible();
  await box.fill("riker@enterprise.gov");

  let connectBody: { account?: string; ephemeral_browser?: boolean } | null = null;
  await page.route("**/api/latchkey/claude-ai/connect", (route) => {
    connectBody = route.request().postDataJSON();
    return route.fulfill({ json: { id: "a1", status: "running", output: "" } });
  });
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect.poll(() => connectBody?.account).toBe("riker@enterprise.gov");

  // And the login must not reuse latchkey's saved session: a cookie
  // capture reads the `Set-Cookie` of a sign-in that then never
  // happens, and waits for it until the 15-minute timeout with an
  // innocent-looking browser window open (imbue-ai/latchkey#150).
  await expect.poll(() => connectBody?.ephemeral_browser).toBe(true);
});

/// Left empty, the box means latchkey's unnamed default, which is
/// addressed by naming no account at all.
test("an empty account box writes no account at all", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);
  await reviewToml(page);
  await expect(wizard(page).locator(".wiz-review pre")).not.toContainText("latchkey_settings");
});

/// An expired sign-in shows up as a failed Check connection, said in
/// one sentence that points at the login button beside it — not at the
/// terminal command the probe's own recipe names, which stays in the
/// details — and a login that then succeeds clears the failure it
/// answered rather than leaving it on screen.
test("a failed Check connection points back at the login button", async ({ page }) => {
  await openClaude(page, WITH_BROWSER);
  await page.route("**/api/probe", (route) =>
    route.fulfill(
      probeFailed(
        "rejected",
        "claude.ai credentials are not set up: GET /api/account -> HTTP 401\n" +
          "The credential is the `sessionKey` cookie.",
      ),
    ),
  );
  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  const failed = wizard(page).locator(".wiz-probe-failed");
  await expect(failed.locator(".issue-headline")).toHaveText(
    "Claude turned the stored sign-in away.",
  );
  await expect(failed).toContainText("Sign in again above");
  await expect(failed.locator("details")).toContainText("sessionKey");

  await page.route("**/api/latchkey/claude-ai/connect", (route) =>
    route.fulfill({ json: { id: "a1", status: "running", output: "" } }),
  );
  await page.route("**/api/latchkey/connect/a1/status", (route) =>
    route.fulfill({ json: { id: "a1", status: "ok", account: null, output: "" } }),
  );
  // A login is checked at once, with the credential it just stored.
  await page.unroute("**/api/probe");
  await page.route("**/api/probe", (route) =>
    route.fulfill(
      probeDone({
        mode: "api",
        account: { id: "u1", address: STORED_ACCOUNT, display_name: null, message_estimate: null },
        items: [],
        notes: [],
      }),
    ),
  );
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
    `Connected as ${STORED_ACCOUNT}`,
  );
  await expect(failed).toHaveCount(0);
});

/// Behind a latchkey gateway (`LATCHKEY_GATEWAY` set — how minds runs
/// datalib) the `latchkey` the backend spawns refuses every command the
/// button would run, and the browser that could sign in is on the
/// gateway's side. The regression: the button was offered, and its
/// failure told the person to run `ensure-browser` — a command that
/// configures a browser on the wrong machine.
test("behind a gateway there is no login button, only where to sign in", async ({ page }) => {
  await openClaude(page, { ...WITH_BROWSER, gateway: "http://gw.example:8080" });

  await expect(wizard(page).getByRole("button", { name: "Sign in with browser" })).toHaveCount(0);
  await expect(wizard(page)).not.toContainText("may log out your other claude.ai session");
  await expect(wizard(page)).not.toContainText("latchkey auth set");
  await expect(wizard(page)).toContainText("held by a latchkey gateway (http://gw.example:8080)");
  // Check connection still works there: the probe goes through
  // `latchkey curl`, which the gateway serves.
  await expect(wizard(page).getByRole("button", { name: "Check connection" })).toBeVisible();
});
