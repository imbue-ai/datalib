// Gmail and Fastmail: two wizard forms over one step type, and the
// Connection block that fills their label pickers from the live account.
//
// **The network is stubbed here on purpose.** The real
// `/api/latchkey/*` and `/api/probe` shell out to latchkey and to
// `datalib-step probe`, which need a credential in the host's keyring
// and reach Google and Fastmail. What this spec is for is the wiring
// between them and the form — that a probe's answer becomes chips, that
// ticking a chip becomes `only_extract_labels`, that the render dialog
// is offered folders rather than flags — and every one of those is a
// pure function of the response body. The response bodies below are
// real ones, trimmed: they were captured from a live probe of a Gmail
// account and a Fastmail mailbox on 2026-09-04.
//
// The fixture root's config.toml is shared by every spec in the run
// (workers: 1), so it is restored in afterEach — including on failure.
import { test, expect, type Page } from "@playwright/test";

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
const chips = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-labelchip`);

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
  mode: "gmail_api",
  account: {
    id: "picard@enterprise.gov",
    address: "picard@enterprise.gov",
    display_name: null,
    message_estimate: 26328,
  },
  labels: [
    { path: "Inbox", kind: "mailbox", role: "inbox", messages: null },
    { path: "Sent", kind: "mailbox", role: "sent", messages: null },
    { path: "Bridge/Logs", kind: "mailbox", role: null, messages: null },
    { path: "Important", kind: "keyword", role: null, messages: null },
    { path: "Starred", kind: "keyword", role: null, messages: null },
    { path: "Unread", kind: "keyword", role: null, messages: null },
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
  labels: [
    { path: "Inbox", kind: "mailbox", role: "inbox", messages: 18 },
    { path: "Sent", kind: "mailbox", role: "sent", messages: 5 },
    { path: "travel", kind: "mailbox", role: null, messages: 5 },
    { path: "travel/portugal", kind: "mailbox", role: null, messages: 5 },
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
    route.fulfill({ json: "gmail_api" in params ? GMAIL_PROBE : FASTMAIL_PROBE });
  });
}

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

async function pickTile(page: Page, query: string, blurb: string) {
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
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
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
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

test("a probe fills the label picker, and ticking a chip writes the filter", async ({
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
  await expect(chips(page, "Download only these labels")).toHaveCount(0);

  await wizard(page).getByRole("button", { name: "Test connection" }).click();
  await expect(wizard(page).locator(".wiz-conn-note")).toContainText(
    "Reached picard@enterprise.gov",
  );

  // What the probe was sent has to be what Save would write: the
  // account, and the table that selects the Gmail download mode.
  expect(lastProbeRequest.type).toBe("email");
  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "picard@enterprise.gov" },
    gmail_api: { user_id: "me" },
  });

  // The download filter may name anything the account has, flags
  // included — Gmail resolves those server-side.
  await expect(chips(page, "Download only these labels")).toHaveText([
    /Inbox/,
    /Sent/,
    /Bridge\/Logs/,
    /Important/,
    /Starred/,
    /Unread/,
  ]);

  await chips(page, "Download only these labels").filter({ hasText: "Bridge/Logs" }).click();
  await chips(page, "Download only these labels").filter({ hasText: "Inbox" }).click();

  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  await expect(toml).toContainText('only_extract_labels = ["Bridge/Logs", "Inbox"]');
  // Presence of the table is what selects the mode; without it the
  // provider falls through to the mbox path and fails at sync time.
  await expect(toml).toContainText("[steps.params.gmail_api]");
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

test("the render step is offered folders, never flags", async ({ page }) => {
  await pickTile(page, "gmail", "Mirror a Gmail account through Google's API.");
  await field(page, "Name").fill("Bridge mail");
  await wizard(page).locator("select.wiz-accountpick").selectOption("picard@enterprise.gov");

  // Email's render step has options, so it comes as a second dialog
  // rather than the checkbox. Decline the chained offer and reach the
  // same form through the row action, so this spec doesn't depend on
  // the confirm() that carries it.
  page.once("dialog", (d) => void d.dismiss());
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Bridge mail.")).toBeVisible();

  await page
    .locator('.ag-row[row-id="bridge-mail/raw"]')
    .getByRole("button", { name: "Render to markdown" })
    .click();

  // A render step holds no credentials. Its probe authenticates with
  // the params of the step it reads — which is why this works at all.
  await wizard(page).getByRole("button", { name: "Test connection" }).click();
  await expect
    .poll(() => lastProbeRequest.params)
    .toEqual({
      latchkey_settings: { account: "picard@enterprise.gov" },
      gmail_api: { user_id: "me" },
    });

  // `Important`, `Starred` and `Unread` are labels on the wire and
  // flags in the schema, so they never become a mailbox row. Offering
  // them here would offer a filter that silently renders nothing.
  await expect(chips(page, "Render only these labels")).toHaveText([
    /Inbox/,
    /Sent/,
    /Bridge\/Logs/,
  ]);

  await chips(page, "Render only these labels").filter({ hasText: "Inbox" }).click();
  await wizard(page).getByRole("button", { name: "Add render step" }).click();
  await expect(page.locator('.ag-row[row-id="bridge-mail/rendered_md"]')).toBeVisible();
  // The outlink is a preset: a Gmail source's webmail links are
  // Gmail's, and there is no second answer to ask about.
  await expect(page.locator(".m2-editor")).toHaveValue(/outlink_format = "gmail"/);
  await expect(page.locator(".m2-editor")).toHaveValue(/only_render_labels = \["Inbox"\]/);
});

test("Fastmail writes its JMAP host without asking, and shows folder counts", async ({
  page,
}) => {
  await pickTile(page, "fastmail", "Mirror a Fastmail mailbox over JMAP.");
  await wizard(page).locator("select.wiz-accountpick").selectOption("troi@betazed.example");
  await wizard(page).getByRole("button", { name: "Test connection" }).click();

  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "troi@betazed.example" },
    sync: { hostname: "api.fastmail.com" },
  });

  // JMAP reports counts for free. Nested folders keep their full path,
  // which is the string the filter matches.
  await expect(chips(page, "Download only these folders")).toHaveText([
    /Inbox\s*18/,
    /Sent\s*5/,
    /travel\s*5/,
    /travel\/portugal\s*5/,
  ]);

  await chips(page, "Download only these folders")
    .filter({ hasText: "travel/portugal" })
    .click();
  await wizard(page).getByText("Review the TOML this writes").click();
  const toml = wizard(page).locator(".wiz-review pre");
  await expect(toml).toContainText('hostname = "api.fastmail.com"');
  await expect(toml).toContainText('only_extract_labels = ["travel/portugal"]');
  await expect(toml).not.toContainText("gmail_api");
});

test("an existing step reopens on the form that wrote it", async ({ page }) => {
  await pickTile(page, "fastmail", "Mirror a Fastmail mailbox over JMAP.");
  await field(page, "Name").fill("Personal mail");
  page.once("dialog", (d) => void d.dismiss());
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Personal mail.")).toBeVisible();

  // Not "Email (mbox or other server)": the step's own params say which
  // variant it is, and a preset with no field must still count as
  // modeled or Edit would be disabled on the wizard's own output.
  await page
    .locator('.ag-row[row-id="personal-mail/raw"]')
    .getByRole("button", { name: "Edit" })
    .click();
  await expect(wizard(page).locator(".wiz-chosen")).toContainText("Fastmail");
});
