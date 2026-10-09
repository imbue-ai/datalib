// A browser login to a service that names its own accounts (Slack,
// Gmail, Garmin, …) stores the credential under whoever signed in, and
// latchkey refuses `--account` naming one it does not hold yet. So the
// wizard sends a name only to refresh a stored account, and follows the
// name the login reports. docs/dev/latchkey.md §"Accounts: who names them".
//
// Read-only: nothing here saves.
import { test, expect, type Page } from "@playwright/test";

const wizard = (page: Page) => page.getByRole("dialog");

const SLACK_SERVICE = {
  service: "slack",
  auth_options: ["browser", "set"],
  set_example: 'latchkey auth set slack -H "Authorization: Bearer xoxb-your-token"',
  accounts: [{ account: "picard", credential_type: "token", credential_status: "valid" }],
  registered: true,
  cli: "/opt/datalib/bin/latchkey",
  gateway: null,
  error: null,
  issue: null,
  installs_plugin: null,
  account_naming: "service",
};

async function openSlack(page: Page) {
  await page.route("**/api/latchkey/slack", (route) => route.fulfill({ json: SLACK_SERVICE }));
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Copy channels and DMs from one Slack workspace." })
    .click();
}

/// Records the body of the one login the wizard starts, and answers its
/// poll with a login that landed on `landed`.
async function stubLogin(page: Page, landed: string) {
  const sent: { account?: string }[] = [];
  await page.route("**/api/latchkey/slack/connect", (route) => {
    sent.push(route.request().postDataJSON());
    return route.fulfill({ json: { id: "s1", status: "running", output: "", phase: "preparing" } });
  });
  await page.route("**/api/latchkey/connect/s1/status", (route) =>
    route.fulfill({
      json: { id: "s1", status: "ok", account: landed, output: "", phase: "signing_in" },
    }),
  );
  return sent;
}

test("a new name is never sent: the login names the account, and the box follows it", async ({
  page,
}) => {
  await openSlack(page);
  const box = wizard(page).getByRole("combobox", { name: "Slack account" });
  await expect(wizard(page).locator(".wiz-account-help")).toContainText(
    "Slack names each account itself when you sign in.",
  );
  await box.fill("riker");
  await expect(wizard(page).locator(".wiz-name-left")).toContainText("riker");

  const sent = await stubLogin(page, "riker in Enterprise");
  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0]?.account).toBe("");
  await expect(box).toHaveValue("riker in Enterprise");
});

test("a stored account is signed in to again by name", async ({ page }) => {
  await openSlack(page);
  await wizard(page).getByRole("combobox", { name: "Slack account" }).fill("picard");
  await expect(wizard(page).locator(".wiz-name-left")).toHaveCount(0);

  const sent = await stubLogin(page, "picard");
  await wizard(page).getByRole("button", { name: "Sign in again as picard" }).click();
  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0]?.account).toBe("picard");
});
