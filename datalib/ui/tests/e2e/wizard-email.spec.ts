// Gmail and Fastmail: two wizard forms over one step type, and the
// Connection block that fills their label pickers from the live account.
import { test, expect, type Page } from "@playwright/test";
import { expandGroup } from "./grid-helpers";

const wizard = (page: Page) => page.getByRole("dialog");
/// A field's own input. Descendant rather than direct child: a
/// `string_list` with a picker wraps its box and its chips in one span,
/// so `> .wiz-input` (which the older specs use for plain fields) finds
/// nothing here. `.first()` keeps it to the text box — the chips are
/// buttons, not inputs, so nothing else matches anyway.
const field = (page: Page, caption: string) =>
  wizard(page)
    .locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-input`)
    .first();
/// The probe-filled picker for one field: an AG Grid, one row per
/// thing the account has, `row-id` being the exact string the filter
/// writes.
const picker = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .pick-grid`);
const rows = (page: Page, caption: string) => picker(page, caption).locator(".ag-row");
/// Tick one row's checkbox. Scoped to the selection column's own cell
/// so it cannot land on a cell that merely contains the text.
const tick = (page: Page, caption: string, id: string) =>
  picker(page, caption)
    .locator(`.ag-row[row-id="${id}"] .ag-selection-checkbox input`)
    .first()
    .click();

/// `latchkey services info google-gmail`, reshaped by the server.
const GMAIL_SERVICE = {
  service: "google-gmail",
  auth_options: ["browser", "set"],
  accounts: [
    { account: "picard@enterprise.gov", credential_type: "oauth", credential_status: "valid" },
    { account: "riker@enterprise.gov", credential_type: "oauth", credential_status: "invalid" },
  ],
  error: null,
};

/// A trimmed real Gmail probe. The three keyword entries are the ones
/// that matter: Gmail returns them as labels, we store them as flags,
/// and so they are downloadable but never renderable.
const GMAIL_PROBE = {
  mode: "gmail",
  account: {
    id: "picard@enterprise.gov",
    address: "picard@enterprise.gov",
    display_name: null,
    message_estimate: 26328,
  },
  items: [
    { path: "Inbox", kind: "mailbox", title: null, role: "inbox", messages: null, updated_at: null },
    { path: "Sent", kind: "mailbox", title: null, role: "sent", messages: null, updated_at: null },
    { path: "Bridge/Logs", kind: "mailbox", title: null, role: null, messages: null, updated_at: null },
    { path: "Important", kind: "keyword", title: null, role: null, messages: null, updated_at: null },
    { path: "Starred", kind: "keyword", title: null, role: null, messages: null, updated_at: null },
    { path: "Unread", kind: "keyword", title: null, role: null, messages: null, updated_at: null },
  ],
  notes: [],
};

const FASTMAIL_SERVICE = {
  service: "fastmail",
  auth_options: ["browser", "set"],
  accounts: [
    { account: "troi@betazed.example", credential_type: "oauth", credential_status: "valid" },
  ],
  error: null,
};

/// A JMAP probe. Every mailbox is a mailbox — JMAP has no keyword-only
/// folders — and `Mailbox/get` reports counts for free, which Gmail
/// does not.
const FASTMAIL_PROBE = {
  mode: "sync",
  account: {
    id: "u432643a7",
    address: "troi@betazed.example",
    display_name: "troi@betazed.example",
    message_estimate: null,
  },
  items: [
    { path: "Inbox", kind: "mailbox", title: null, role: "inbox", messages: 18, updated_at: null },
    { path: "Sent", kind: "mailbox", title: null, role: "sent", messages: 5, updated_at: null },
    { path: "travel", kind: "mailbox", title: null, role: null, messages: 5, updated_at: null },
    { path: "travel/portugal", kind: "mailbox", title: null, role: null, messages: 5, updated_at: null },
  ],
  notes: [],
};

/// Whatever the last probe was asked to authenticate with. Asserted on
/// rather than merely stubbed: a probe sent the wrong params comes back
/// looking perfectly healthy while describing a different mailbox.
let lastProbeRequest: { type?: string; params?: Record<string, unknown> } = {};

async function stubBackend(page: Page) {
  await page.route("**/api/latchkey/google-gmail", (route) =>
    route.fulfill({ json: GMAIL_SERVICE }),
  );
  await page.route("**/api/latchkey/fastmail", (route) =>
    route.fulfill({ json: FASTMAIL_SERVICE }),
  );
  await page.route("**/api/probe", (route) => {
    lastProbeRequest = route.request().postDataJSON();
    const params = (lastProbeRequest.params ?? {}) as Record<string, unknown>;
    route.fulfill({ json: "gmail" in params ? GMAIL_PROBE : FASTMAIL_PROBE });
  });
}

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

async function pickTile(page: Page, query: string, blurb: string) {
  await page.getByRole("button", { name: "+ Data Source" }).click();
  await page.getByRole("searchbox").fill(query);
  await wizard(page).locator(".wiz-tile", { hasText: blurb }).click();
}

let original = "";

test.beforeEach(async ({ page }) => {
  lastProbeRequest = {};
  await stubBackend(page);
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});

test("Gmail and Fastmail are separate tiles over one step type", async ({ page }) => {
  await page.getByRole("button", { name: "+ Data Source" }).click();
  await page.getByRole("searchbox").fill("mail");
  // Matched on the blurb, not the label: the catch-all's blurb names
  // Fastmail too ("a JMAP server other than Fastmail"), so filtering on
  // the word alone resolves to two tiles.
  const tiles = wizard(page).locator(".wiz-tile");
  await expect(
    tiles.filter({ hasText: "Mirror a Gmail account through Google's API." }),
  ).toBeEnabled();
  await expect(tiles.filter({ hasText: "Mirror a Fastmail mailbox over JMAP." })).toBeEnabled();
  // The catch-all stays in the picker for the modes that have no form —
  // an mbox, a JMAP server that isn't Fastmail — and stays disabled.
  await expect(
    tiles.filter({ hasText: "A Google Takeout .mbox, or a JMAP server other than Fastmail." }),
  ).toBeDisabled();
});

test("a probe fills the label picker, and ticking a row writes the filter", async ({
  page,
}) => {
  await pickTile(page, "gmail", "Mirror a Gmail account through Google's API.");

  // The account list comes from latchkey, and the dropdown says which
  // credentials are actually usable.
  const account = wizard(page).locator("select.wiz-accountpick");
  await expect(account.locator("option")).toHaveText([
    "Type an account…",
    /picard@enterprise\.gov ✓/,
    /riker@enterprise\.gov — expired/,
  ]);
  await account.selectOption("picard@enterprise.gov");

  // Before the probe there is nothing to pick from, and the form says
  // so rather than showing an empty box.
  await expect(picker(page, "Download only these labels")).toHaveCount(0);

  await wizard(page).getByRole("button", { name: "Test connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-note")).toContainText(
    "Reached picard@enterprise.gov",
  );

  // What the probe was sent has to be what Save would write: the
  // account, and the table that selects the Gmail download mode.
  expect(lastProbeRequest.type).toBe("email");
  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "picard@enterprise.gov" },
    gmail: { user_id: "me" },
  });

  // The download filter may name anything the account has, flags
  // included — Gmail resolves those server-side.
  await expect(rows(page, "Download only these labels")).toHaveText([
    /Inbox/,
    /Sent/,
    /Bridge\/Logs/,
    /Important/,
    /Starred/,
    /Unread/,
  ]);

  await tick(page, "Download only these labels", "Bridge/Logs");
  await tick(page, "Download only these labels", "Inbox");

  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  await expect(toml).toContainText('only_extract_labels = ["Bridge/Logs", "Inbox"]');
  // Presence of the table is what selects the mode; without it the
  // step names no method and is refused at sync time.
  await expect(toml).toContainText("[steps.params.gmail]");
  await expect(toml).toContainText('account = "picard@enterprise.gov"');
});

test("a typed label the account doesn't have is called out before saving", async ({ page }) => {
  await pickTile(page, "gmail", "Mirror a Gmail account through Google's API.");
  await field(page, "Download only these labels").fill("Inbox, Bridg/Logs");
  await wizard(page).getByRole("button", { name: "Test connection" }).click();

  // Gmail's downloader *refuses* a run whose filter names a label the
  // account lacks — an empty filter would mean "everything", so it
  // cannot fall back. Much cheaper to find here.
  await expect(wizard(page).getByText(/Not on this account: Bridg\/Logs/)).toBeVisible();
});

test("the render filter is offered folders, never flags", async ({ page }) => {
  await pickTile(page, "gmail", "Mirror a Gmail account through Google's API.");
  await field(page, "Name").fill("Bridge mail");
  await wizard(page).locator("select.wiz-accountpick").selectOption("picard@enterprise.gov");

  // One probe fills both pickers: the ingest step's, and the render
  // step's under the Rendering heading. The render step holds no
  // credentials of its own — the probe authenticates with what the
  // ingest step will write.
  await wizard(page).getByRole("button", { name: "Test connection" }).click();
  await expect
    .poll(() => lastProbeRequest.params)
    .toEqual({
      latchkey_settings: { account: "picard@enterprise.gov" },
      gmail: { user_id: "me" },
    });

  // `Important`, `Starred` and `Unread` are labels on the wire and
  // flags in the schema, so they never become a mailbox row. Offering
  // them here would offer a filter that silently renders nothing.
  await expect(rows(page, "Render only these labels")).toHaveText([
    /Inbox/,
    /Sent/,
    /Bridge\/Logs/,
  ]);

  await tick(page, "Render only these labels", "Inbox");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Bridge mail.")).toBeVisible();
  await expandGroup(page, "bridge-mail");
  await expect(page.locator('.ag-row[row-id="bridge-mail/render_markdown"]')).toBeVisible();
  // The outlink is a preset: a Gmail source's webmail links are
  // Gmail's, and there is no second answer to ask about.
  await expect(page.locator(".m2-editor")).toHaveValue(/outlink_format = "gmail"/);
  await expect(page.locator(".m2-editor")).toHaveValue(/only_render_labels = \["Inbox"\]/);
  // ...and it landed on the render step, not the ingest step.
  const text = await page.locator(".m2-editor").inputValue();
  expect(text.indexOf("only_render_labels")).toBeGreaterThan(
    text.indexOf('function = "render_markdown"'),
  );
});

test("Fastmail writes its JMAP host without asking, and shows folder counts", async ({
  page,
}) => {
  await pickTile(page, "fastmail", "Mirror a Fastmail mailbox over JMAP.");
  await wizard(page).locator("select.wiz-accountpick").selectOption("troi@betazed.example");
  await wizard(page).getByRole("button", { name: "Test connection" }).click();

  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "troi@betazed.example" },
    jmap: { hostname: "api.fastmail.com" },
  });

  // JMAP reports counts for free. Nested folders keep their full path,
  // which is the string the filter matches.
  await expect(rows(page, "Download only these folders")).toHaveText([
    /Inbox.*18/,
    /Sent.*5/,
    /travel.*5/,
    /travel\/portugal.*5/,
  ]);

  await tick(page, "Download only these folders", "travel/portugal");
  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  await expect(toml).toContainText('hostname = "api.fastmail.com"');
  await expect(toml).toContainText('only_extract_labels = ["travel/portugal"]');
  await expect(toml).not.toContainText("gmail");
});

/// Signing in is how a second account comes to exist, so the form has
/// to follow the login rather than the box. latchkey ignores
/// `--account` when it stores and files under the address actually
/// signed in with (imbue-ai/latchkey#148) — it just reports which. A
/// form left naming the old account would write a config pointing at a
/// credential that is not there.
test("the account follows the login, not the box", async ({ page }) => {
  await pickTile(page, "fastmail", "Mirror a Fastmail mailbox over JMAP.");
  await wizard(page).locator("select.wiz-accountpick").selectOption("troi@betazed.example");

  await page.route("**/api/latchkey/fastmail/connect", (route) =>
    route.fulfill({ json: { id: "c1", status: "running", account: null, output: "" } }),
  );
  // What `auth browser` reports for an OAuth service: a different
  // address from the one selected above, because that is who signed in.
  await page.route("**/api/latchkey/connect/c1/status", (route) =>
    route.fulfill({
      json: { id: "c1", status: "ok", account: "crusher@enterprise.gov", output: "Done." },
    }),
  );

  await wizard(page).getByRole("button", { name: "Latchkey auth" }).click();
  await expect(wizard(page).locator(".wiz-conn-note")).toContainText(
    "Connected as crusher@enterprise.gov",
  );
  // The text box, not the dropdown beside it: both carry `.wiz-input`,
  // and the dropdown reads `__other` because a just-created account is
  // not in the list this stub keeps returning.
  await expect(
    wizard(page).locator('.wiz-field:has(> .wiz-label:text-is("Fastmail account")) input.wiz-input'),
  ).toHaveValue("crusher@enterprise.gov");

  // …and that is what the config gets, not the address that was picked.
  await wizard(page).getByText("Review the TOML this writes").click();
  await expect(wizard(page).locator(".wiz-review pre")).toContainText(
    'account = "crusher@enterprise.gov"',
  );
});

test("an existing source reopens on the form that wrote it", async ({ page }) => {
  await pickTile(page, "fastmail", "Mirror a Fastmail mailbox over JMAP.");
  await field(page, "Name").fill("Personal mail");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Personal mail.")).toBeVisible();

  // Not "Email (mbox or other server)": the ingest step's own params say
  // which variant it is, and a preset with no field — on either step —
  // must still count as modeled, or Edit would be disabled on the
  // wizard's own output.
  await expandGroup(page, "personal-mail");
  await page
    .locator('.ag-row[row-id="personal-mail/render_markdown"]')
    .getByRole("button", { name: "Edit" })
    .click();
  await expect(wizard(page).locator(".wiz-chosen")).toContainText("Fastmail");
});
