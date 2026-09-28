// The Connection block's "Paste a credential" form: what it sends to
// `POST /api/latchkey/<service>/credential`, which the server turns into
// `latchkey auth set`. Two shapes — an app password for Fastmail's DAV,
// a header for Fastmail's JMAP — and the one trap: an unnamed paste
// replaces the credential latchkey already holds.
//
// Read-only: every write is routed to a stub, so it runs against the
// shared fixture root rather than a sandbox of its own.
import { test, expect, type Page } from "@playwright/test";

const wizard = (page: Page) => page.getByRole("dialog");
const pasteForm = (page: Page) => wizard(page).locator(".wiz-paste");

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
  await page.getByRole("button", { name: "+ Data Source" }).click();
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
    return route.fulfill({ json: REPORT });
  });
  await openTile(
    page,
    FASTMAIL_DAV,
    "fastmail-dav",
    "Mirror a Fastmail account's address books over CardDAV.",
  );

  // Set-only: the form is the next step, so it is already open.
  const form = pasteForm(page);
  await expect(form).toContainText("App passwords");
  await form.getByLabel("Username").fill("picard@enterprise.test");
  await form.getByLabel("App password").fill("tea-earl-grey");
  await form.getByRole("button", { name: "Store in latchkey" }).click();

  await expect
    .poll(() => sent)
    .toEqual({
      account: "",
      credential: { kind: "basic", username: "picard@enterprise.test", password: "tea-earl-grey" },
    });
  await expect(form).toContainText("Stored in latchkey.");
  await expect(form.getByLabel("App password")).toHaveValue("");
  await expect.poll(() => probes).toBe(1);
});

test("a read-only token does not silently replace the browser login", async ({ page }) => {
  let sent: Sent | null = null;
  await page.route("**/api/latchkey/fastmail/credential", (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/probe", (route) => route.fulfill({ json: REPORT }));
  await openTile(page, FASTMAIL_JMAP, "fastmail", "Mirror a Fastmail mailbox over JMAP.");

  await wizard(page).getByRole("button", { name: "Paste a credential" }).click();
  const form = pasteForm(page);
  await expect(form).toContainText("Read-only access");
  await expect(form).toContainText("Authorization: Bearer …");
  await form.getByLabel("Token").fill("ro-token");
  await expect(form).toContainText(
    "This replaces the credential stored for picard@enterprise.test",
  );

  await form.getByLabel("Store under account").fill("picard-readonly");
  await expect(form).not.toContainText("This replaces");
  await form.getByRole("button", { name: "Store in latchkey" }).click();

  await expect
    .poll(() => sent)
    .toEqual({
      account: "picard-readonly",
      credential: { kind: "headers", headers: ["Authorization: Bearer ro-token"] },
    });
  // The source now names the account the token was stored under.
  await expect(wizard(page).locator(".wiz-accountrow input")).toHaveValue("picard-readonly");
});

test("under a latchkey gateway nothing is pasted here", async ({ page }) => {
  await openTile(
    page,
    { ...FASTMAIL_DAV, gateway: "https://gateway.enterprise.test" },
    "fastmail-dav",
    "Mirror a Fastmail account's address books over CardDAV.",
  );
  await expect(wizard(page)).toContainText("gateway.enterprise.test");
  await expect(wizard(page).getByRole("button", { name: "Paste a credential" })).toHaveCount(0);
  await expect(pasteForm(page)).toHaveCount(0);
});
