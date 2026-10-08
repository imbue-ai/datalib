// The account row's "Paste a key" form: what it sends to
// `POST /api/latchkey/<service>/credential`, which the server turns into
// `latchkey auth set`. Two shapes — an app password for Fastmail's DAV,
// a header for Fastmail's JMAP — stored under the name in the account
// box, which can replace a credential latchkey already holds.
//
// Read-only: every write is routed to a stub, so it runs against the
// shared fixture root rather than a sandbox of its own.
import { test, expect, type Page } from "@playwright/test";
import { reviewToml, showSignIn } from "./wizard-helpers";
import { probeDone } from "./probe-stub";

const wizard = (page: Page) => page.getByRole("dialog");
const pasteForm = (page: Page) => wizard(page).locator(".wiz-paste");
const accountBox = (page: Page) => wizard(page).getByRole("combobox", { name: "Fastmail account" });

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

const FASTMAIL_JMAP = {
  service: "fastmail",
  auth_options: ["browser", "set"],
  set_example: 'latchkey auth set fastmail -H "Authorization: Bearer <token>"',
  accounts: [
    { account: "picard@enterprise.test", credential_type: "oauth", credential_status: "valid" },
  ],
  registered: true,
  cli: "/opt/datalib/bin/latchkey",
  gateway: null,
  error: null,
};

const REPORT = {
  mode: "fastmail",
  account: {
    id: "picard@enterprise.test",
    address: "picard@enterprise.test",
    display_name: null,
    message_estimate: null,
  },
  items: [
    {
      path: "Bridge",
      kind: "address_book",
      title: null,
      role: null,
      messages: null,
      members: null,
      updated_at: null,
    },
  ],
  notes: [],
};

async function openTile(page: Page, service: object, name: string, blurb: string) {
  await page.route(`**/api/latchkey/${name}`, (route) => route.fulfill({ json: service }));
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  await wizard(page).locator(".wiz-tile", { hasText: blurb }).click();
}

type Sent = { account: string; credential: Record<string, unknown> };

test("an app password is pasted, stored and then tested", async ({ page }) => {
  let sent: Sent | null = null;
  let probes = 0;
  await page.route("**/api/latchkey/fastmail-dav/credential", (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/probe", (route) => {
    probes += 1;
    return route.fulfill(probeDone(REPORT));
  });
  await openTile(
    page,
    FASTMAIL_DAV,
    "fastmail-dav",
    "Copy a Fastmail account's address books over CardDAV.",
  );

  // fastmail-dav has no web login, so the paste form is all there is:
  // no tabs, nothing to open.
  await expect(wizard(page).getByRole("tab")).toHaveCount(0);
  const form = pasteForm(page);
  await expect(form).toContainText("App passwords");
  await form.getByLabel("Username").fill("picard@enterprise.test");
  await form.getByLabel("App password").fill("tea-earl-grey");
  // The account box follows the username, plus the entry's suffix, until
  // something is typed or picked there.
  await expect(accountBox(page)).toHaveValue("picard@enterprise.test contacts");
  await expect(form).toContainText("Stored as picard@enterprise.test contacts.");
  await form.getByRole("button", { name: "Store in latchkey" }).click();

  await expect
    .poll(() => sent)
    .toEqual({
      account: "picard@enterprise.test contacts",
      credential: { kind: "basic", username: "picard@enterprise.test", password: "tea-earl-grey" },
    });
  // Stored, then checked at once: the row says who the password reaches.
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
    "Connected as picard@enterprise.test",
  );
  await expect.poll(() => probes).toBe(1);
});

test("a read-only token does not silently replace the browser login", async ({ page }) => {
  let sent: Sent | null = null;
  await page.route("**/api/latchkey/fastmail/credential", (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/probe", (route) => route.fulfill(probeDone(REPORT)));
  await openTile(page, FASTMAIL_JMAP, "fastmail", "Copy a Fastmail mailbox over JMAP.");

  // Both ways in, the browser login first; the paste tab says how to get less.
  await showSignIn(page);
  await expect(wizard(page).getByRole("tab", { name: "Web login" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await wizard(page).getByRole("tab", { name: "Paste a key" }).click();
  const form = pasteForm(page);
  await expect(form).toContainText("Read-only access");
  await expect(form).toContainText("Authorization: Bearer …");
  await form.getByLabel("Token").fill("ro-token");
  // A token has no username to name it after, so it waits for a name.
  await expect(form.getByRole("button", { name: "Store in latchkey" })).toBeDisabled();
  await accountBox(page).fill("picard@enterprise.test");
  await expect(form).toContainText("Stored as picard@enterprise.test, replacing");

  await accountBox(page).fill("picard-readonly");
  await expect(form).not.toContainText("replacing");
  await form.getByRole("button", { name: "Store in latchkey" }).click();

  await expect
    .poll(() => sent)
    .toEqual({
      account: "picard-readonly",
      credential: { kind: "headers", headers: ["Authorization: Bearer ro-token"] },
    });
  // The source names the account the token was stored under.
  const toml = await reviewToml(page);
  await expect(toml).toContainText('account = "picard-readonly"');
});

test("a named paste beside the unnamed credential says what it strands", async ({ page }) => {
  await openTile(
    page,
    {
      ...FASTMAIL_DAV,
      accounts: [{ account: "", credential_type: "rawCurl", credential_status: "valid" }],
    },
    "fastmail-dav",
    "Copy a Fastmail account's address books over CardDAV.",
  );
  const form = pasteForm(page);
  await form.getByLabel("Username").fill("picard@enterprise.test");
  await expect(form).toContainText("also holds an unnamed fastmail-dav credential");
});

test("under a latchkey gateway nothing is pasted here", async ({ page }) => {
  await openTile(
    page,
    { ...FASTMAIL_DAV, gateway: "https://gateway.enterprise.test" },
    "fastmail-dav",
    "Copy a Fastmail account's address books over CardDAV.",
  );
  await expect(wizard(page)).toContainText("gateway.enterprise.test");
  await expect(wizard(page).getByRole("tab", { name: "Paste a key" })).toHaveCount(0);
  await expect(pasteForm(page)).toHaveCount(0);
});
