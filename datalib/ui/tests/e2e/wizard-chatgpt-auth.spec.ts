// The Connection block for ChatGPT, whose latchkey login is a token
// capture rather than Claude's cookie capture.
//
// The service is the common case for an old install: `chatgpt` was
// registered by hand before latchkey had a browser login for it, so it
// is `set`-only and the wizard's job is to show the conversion — with
// the token-capture registration, not Claude's cookie one — rather than
// to take the service apart on a click. `wizard-claude-auth.spec.ts`
// covers the rest of the block; only what differs by flow is here.
//
// Read-only: nothing here saves, so it runs against the shared fixture
// root rather than a sandbox of its own.
import { test, expect, type Page } from "@playwright/test";

const wizard = (page: Page) => page.getByRole("dialog");

const SET_ONLY = {
  service: "chatgpt",
  auth_options: ["set"],
  accounts: [{ account: "", credential_type: "rawCurl", credential_status: "unknown" }],
  registered: true,
  cli: "/opt/datalib/bin/latchkey",
  error: null,
};

const WITH_BROWSER = { ...SET_ONLY, auth_options: ["browser", "set"] };

async function openChatgpt(page: Page, service: object) {
  await page.route("**/api/latchkey/chatgpt", (route) => route.fulfill({ json: service }));
  await page.goto("/sources2");
  await page.getByRole("button", { name: "+ Data Source" }).click();
  await wizard(page).locator(".wiz-tile", { hasText: "Mirror your ChatGPT conversations" }).click();
}

test("a set-only chatgpt service is shown the token-capture conversion", async ({ page }) => {
  await openChatgpt(page, SET_ONLY);

  let connectCalls = 0;
  await page.route("**/api/latchkey/chatgpt/connect", (route) => {
    connectCalls += 1;
    return route.fulfill({ json: { id: "x", status: "running", output: "" } });
  });
  await wizard(page).getByRole("button", { name: "Latchkey auth" }).click();

  const commands = wizard(page).locator(".wiz-convert pre");
  await expect(commands).toContainText("/opt/datalib/bin/latchkey auth clear chatgpt --all");
  await expect(commands).toContainText("services deregister chatgpt");
  await expect(commands).toContainText('--login-url="https://chatgpt.com/auth/login"');
  await expect(commands).toContainText("--login-flow=token-capture");
  await expect(commands).toContainText(
    '--login-flow-params=\'{"tokenUrl":"https://chatgpt.com/api/auth/session","tokenField":"accessToken"}\'',
  );
  expect(connectCalls).toBe(0);
});

/// A token capture reads the token off a page that is already signed
/// in, so unlike a cookie capture it must *not* throw latchkey's saved
/// session away: with a fresh profile the person signs in again for
/// nothing, and chatgpt.com's sign-in is the one most likely to stall
/// on a bot check.
test("the browser login keeps latchkey's saved session", async ({ page }) => {
  await openChatgpt(page, WITH_BROWSER);

  let connectBody: { account?: string; ephemeral_browser?: boolean } | null = null;
  await page.route("**/api/latchkey/chatgpt/connect", (route) => {
    connectBody = route.request().postDataJSON();
    return route.fulfill({ json: { id: "a1", status: "running", output: "" } });
  });
  await wizard(page).getByRole("button", { name: "Latchkey auth" }).click();

  await expect.poll(() => connectBody?.account).toBe("");
  await expect.poll(() => connectBody?.ephemeral_browser).toBe(false);
});
