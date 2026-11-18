// A latchkey run from a terminal signs in to `~/.latchkey`, never to a
// protected library, so in one the wizard offers no command to run. The
// same wizard in an ordinary library still does, which keeps these from
// passing because the text simply is not drawn.
//
// Read-only: the service lookup and the health answer are stubbed.
import { test, expect, type Page } from "@playwright/test";
import { showSignIn } from "./wizard-helpers";

const wizard = (page: Page) => page.getByRole("dialog");

const FASTMAIL_DAV = {
  service: "fastmail-dav",
  auth_options: ["set"],
  set_example: 'latchkey auth set fastmail-dav -u "you@fastmail.com:<app password>"',
  accounts: [],
  registered: true,
  cli: "/opt/datalib/bin/latchkey",
  gateway: null,
  error: null,
};

async function openWizard(page: Page, protectedLibrary: boolean) {
  await page.route("**/api/health", async (route) => {
    const response = await route.fetch();
    const json = { ...(await response.json()), protected_library: protectedLibrary };
    await route.fulfill({ response, json });
  });
  await page.route("**/api/latchkey/fastmail-dav", (route) =>
    route.fulfill({ json: FASTMAIL_DAV }),
  );
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
}

const FASTMAIL_TILE = "Copy a Fastmail account's address books over CardDAV.";
const CALDAV_TILE = "Copy the calendars on any CalDAV server: iCloud, Nextcloud, Radicale, ….";

test("a protected library offers no terminal command for a sign-in", async ({ page }) => {
  await openWizard(page, true);
  await wizard(page).locator(".wiz-tile", { hasText: FASTMAIL_TILE }).click();
  await showSignIn(page);

  await expect(wizard(page).locator(".wiz-conn-intro")).toContainText(
    "in this library’s protected folder",
  );
  await expect(wizard(page).locator(".wiz-conn-how")).toContainText("latchkey keeps them");
  await expect(wizard(page).locator(".wiz-conn-how")).not.toContainText("auth set");
  await expect(wizard(page).locator(".wiz-paste")).toBeVisible();
});

test("an ordinary library still offers the terminal command", async ({ page }) => {
  await openWizard(page, false);
  await wizard(page).locator(".wiz-tile", { hasText: FASTMAIL_TILE }).click();
  await showSignIn(page);

  await expect(wizard(page).locator(".wiz-conn-intro")).toContainText(
    "outside this library’s folder",
  );
  await expect(wizard(page).locator(".wiz-conn-how")).toContainText(
    "/opt/datalib/bin/latchkey auth set fastmail-dav",
  );
});

test("a source set up only from a terminal says it cannot sign in here", async ({ page }) => {
  await openWizard(page, true);
  await wizard(page).locator(".wiz-tile", { hasText: CALDAV_TILE }).click();

  const before = wizard(page).locator(".wiz-before");
  await expect(before).toContainText("A protected library can only sign in from this dialog");
  await expect(before).not.toContainText("latchkey services register");
});

test("in an ordinary library that source still gives the terminal steps", async ({ page }) => {
  await openWizard(page, false);
  await wizard(page).locator(".wiz-tile", { hasText: CALDAV_TILE }).click();

  await expect(wizard(page).locator(".wiz-before")).toContainText("latchkey services register");
});
