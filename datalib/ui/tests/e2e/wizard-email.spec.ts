// Gmail and Fastmail: two wizard forms over one step type, the
// account row's check, and each label picker's own list.
import { test, expect, type Page } from "@playwright/test";
import { openAdvanced, reviewToml, row, showSignIn } from "./wizard-helpers";
import { expandGroup, pickRowMenu, MANAGE_WITH_CONFIG, savedConfig } from "./grid-helpers";
import { probeAsk, probeDone, reportFor, type ProbeAsk } from "./probe-stub";

const wizard = (page: Page) => page.getByRole("dialog");
/// A field's own input. Descendant rather than direct child: a
/// `string_list` with a picker wraps its box and its chips in one span,
/// so `> .wiz-input` (which the older specs use for plain fields) finds
/// nothing here. `.first()` keeps it to the text box — the chips are
/// buttons, not inputs, so nothing else matches anyway.
const field = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-input`).first();
/// The probe-filled picker under one heading: a grid, one row per
/// thing the account has, `data-key` being the exact string the filter
/// writes.
const picker = (page: Page, heading: string) => row(page, heading).locator(".pick-grid");
const rows = (page: Page, heading: string) => picker(page, heading).locator(".slick-row");
/// The box a list is typed into.
const typed = (page: Page, heading: string) =>
  row(page, heading).locator(".wiz-listfield > .wiz-input");
/// Choosing an answer that shows a list loads it from the account.
const answer = (page: Page, name: string) => wizard(page).getByRole("radio", { name }).check();
/// The button that loads an advanced field's picker from the account.
const load = (page: Page, heading: string) => row(page, heading).locator(".wiz-load-btn").click();
const MAIL = "Which mail?";
/// Tick one row's checkbox — its label, which is what is drawn; the
/// input itself is hidden. Scoped to the selection column's own cell
/// so it cannot land on a cell that merely contains the text.
const tick = (page: Page, caption: string, id: string) =>
  picker(page, caption)
    .locator(`.slick-row[data-key="${id}"] .slick-cell-checkboxsel label`)
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
    {
      path: "Inbox",
      kind: "mailbox",
      title: null,
      role: "inbox",
      messages: null,
      updated_at: null,
    },
    { path: "Sent", kind: "mailbox", title: null, role: "sent", messages: null, updated_at: null },
    {
      path: "Bridge/Logs",
      kind: "mailbox",
      title: null,
      role: null,
      messages: null,
      updated_at: null,
    },
    {
      path: "Important",
      kind: "keyword",
      title: null,
      role: null,
      messages: null,
      updated_at: null,
    },
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
    {
      path: "travel/portugal",
      kind: "mailbox",
      title: null,
      role: null,
      messages: 5,
      updated_at: null,
    },
  ],
  notes: [],
};

/// Whatever the last probe was asked to authenticate with. Asserted on
/// rather than merely stubbed: a probe sent the wrong params comes back
/// looking perfectly healthy while describing a different mailbox.
let lastProbeRequest: ProbeAsk = {};

/// Both email lists are the one folder listing; the picker filters it.
const EMAIL_LISTS = { labels: ["mailbox", "keyword"], mailboxes: ["mailbox", "keyword"] };

async function stubBackend(page: Page) {
  await page.route("**/api/latchkey/google-gmail", (route) =>
    route.fulfill({ json: GMAIL_SERVICE }),
  );
  await page.route("**/api/latchkey/fastmail", (route) =>
    route.fulfill({ json: FASTMAIL_SERVICE }),
  );
  await page.route("**/api/probe", (route) => {
    lastProbeRequest = probeAsk(route);
    const params = lastProbeRequest.params ?? {};
    const report = "gmail" in params ? GMAIL_PROBE : FASTMAIL_PROBE;
    route.fulfill(probeDone(reportFor(report, lastProbeRequest, EMAIL_LISTS)));
  });
}

async function openManager(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
}

/// Opens the account box's list and picks a stored name from it.
async function pickAccount(page: Page, name: string) {
  await showSignIn(page);
  await wizard(page)
    .getByRole("combobox", { name: / account$/ })
    .click();
  await wizard(page)
    .getByRole("listbox", { name: / account$/ })
    .getByRole("option")
    .filter({ hasText: name })
    .click();
}

async function pickTile(page: Page, query: string, blurb: string) {
  await page.getByRole("button", { name: "Add source" }).click();
  await page.locator(".wiz-filter").fill(query);
  await wizard(page).locator(".wiz-tile", { hasText: blurb }).click();
}

let original = "";

test.beforeEach(async ({ page, request }) => {
  lastProbeRequest = {};
  await stubBackend(page);
  await openManager(page);
  original = await savedConfig(request);
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await page.locator(".m2-editor").fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});

test("Gmail and Fastmail are separate tiles over one step type", async ({ page }) => {
  await page.getByRole("button", { name: "Add source" }).click();
  await page.locator(".wiz-filter").fill("mail");
  // Matched on the blurb, not the label: the catch-all's blurb names
  // Fastmail too ("a JMAP server other than Fastmail"), so filtering on
  // the word alone resolves to two tiles.
  const tiles = wizard(page).locator(".wiz-tile");
  await expect(
    tiles.filter({ hasText: "Copy a Gmail account through Google's API." }),
  ).toBeEnabled();
  await expect(tiles.filter({ hasText: "Copy a Fastmail mailbox over JMAP." })).toBeEnabled();
  // The catch-all stays in the picker for the modes that have no form —
  // an mbox, a JMAP server that isn't Fastmail — and stays disabled.
  await expect(
    tiles.filter({ hasText: "A Google Takeout .mbox, or a JMAP server other than Fastmail." }),
  ).toBeDisabled();
});

test("a check names the account; Load fills the label picker, and ticking writes the filter", async ({
  page,
}) => {
  await pickTile(page, "gmail", "Copy a Gmail account through Google's API.");

  // The account list comes from latchkey, and says which credentials
  // latchkey thinks are usable.
  await wizard(page)
    .getByRole("combobox", { name: / account$/ })
    .click();
  await expect(
    wizard(page)
      .getByRole("listbox", { name: / account$/ })
      .getByRole("option"),
  ).toHaveText([/picard@enterprise\.gov\s*✓/, /riker@enterprise\.gov\s*reported invalid/]);
  await wizard(page)
    .getByRole("listbox", { name: / account$/ })
    .getByRole("option")
    .filter({ hasText: "picard@enterprise.gov" })
    .click();
  await expect(wizard(page).getByRole("combobox", { name: / account$/ })).toHaveValue(
    "picard@enterprise.gov",
  );

  // Until an answer asks for a list there is nothing to pick from.
  await expect(picker(page, MAIL)).toHaveCount(0);

  // The check asks for the account alone, and fills no picker.
  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText(
    "Connected as picard@enterprise.gov",
  );
  expect(lastProbeRequest.list).toBeNull();
  await expect(picker(page, MAIL)).toHaveCount(0);

  await answer(page, "Only the labels I choose");
  // What the probe was sent has to be what Save would write: the
  // account, and the table that selects the Gmail download mode.
  await expect.poll(() => lastProbeRequest.list).toBe("labels");
  expect(lastProbeRequest.type).toBe("email");
  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "picard@enterprise.gov" },
    gmail: { user_id: "me" },
  });

  // The download filter may name anything the account has, flags
  // included — Gmail resolves those server-side.
  await expect(rows(page, MAIL)).toHaveText([
    /Inbox/,
    /Sent/,
    /Bridge\/Logs/,
    /Important/,
    /Starred/,
    /Unread/,
  ]);

  await tick(page, MAIL, "Bridge/Logs");
  await tick(page, MAIL, "Inbox");

  await reviewToml(page);
  const toml = wizard(page).locator(".wiz-review pre");
  await expect(toml).toContainText('only_extract_labels = ["Bridge/Logs", "Inbox"]');
  // Presence of the table is what selects the mode; without it the
  // step names no method and is refused at sync time.
  await expect(toml).toContainText("[steps.params.gmail]");
  await expect(toml).toContainText('account = "picard@enterprise.gov"');
});

test("a typed label the account doesn't have is called out before saving", async ({ page }) => {
  await pickTile(page, "gmail", "Copy a Gmail account through Google's API.");
  await answer(page, "Only the labels I choose");
  await typed(page, MAIL).fill("Inbox, Bridg/Logs");

  // Gmail's downloader *refuses* a run whose filter names a label the
  // account lacks — an empty filter would mean "everything", so it
  // cannot fall back. Much cheaper to find here.
  await expect(wizard(page).getByText(/Not on this account: Bridg\/Logs/)).toBeVisible();
});

test("the render filter is offered folders, never flags", async ({ page }) => {
  await pickTile(page, "gmail", "Copy a Gmail account through Google's API.");
  await field(page, "Name").fill("Bridge mail");
  await pickAccount(page, "picard@enterprise.gov");

  // The render step's picker loads its own list. The render step holds
  // no credentials of its own — the probe authenticates with what the
  // ingest step will write.
  await openAdvanced(page);
  await load(page, "Render only these labels");
  await expect.poll(() => lastProbeRequest.list).toBe("mailboxes");
  expect(lastProbeRequest.params).toEqual({
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
  await expect(
    page.locator('.tg-grid .slick-row:not([data-pinned])[data-key="bridge-mail/render_markdown"]'),
  ).toBeVisible();
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

test("Fastmail writes its JMAP host without asking, and shows folder counts", async ({ page }) => {
  await pickTile(page, "fastmail", "Copy a Fastmail mailbox over JMAP.");
  await pickAccount(page, "troi@betazed.example");
  await answer(page, "Only the folders I choose");

  await expect.poll(() => lastProbeRequest.list).toBe("labels");
  expect(lastProbeRequest.params).toEqual({
    latchkey_settings: { account: "troi@betazed.example" },
    jmap: { hostname: "api.fastmail.com" },
  });

  // JMAP reports counts for free. Nested folders keep their full path,
  // which is the string the filter matches.
  await expect(rows(page, MAIL)).toHaveText([
    /Inbox.*18/,
    /Sent.*5/,
    /travel.*5/,
    /travel\/portugal.*5/,
  ]);

  await tick(page, MAIL, "travel/portugal");
  await reviewToml(page);
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
  await pickTile(page, "fastmail", "Copy a Fastmail mailbox over JMAP.");
  await pickAccount(page, "troi@betazed.example");

  let connectBody: { ephemeral_browser?: boolean } | null = null;
  await page.route("**/api/latchkey/fastmail/connect", (route) => {
    connectBody = route.request().postDataJSON();
    return route.fulfill({ json: { id: "c1", status: "running", account: null, output: "" } });
  });
  // What `auth browser` reports for an OAuth service: a different
  // address from the one selected above, because that is who signed in.
  await page.route("**/api/latchkey/connect/c1/status", (route) =>
    route.fulfill({
      json: { id: "c1", status: "ok", account: "crusher@enterprise.gov", output: "Done." },
    }),
  );

  await wizard(page).getByRole("button", { name: "Sign in with browser" }).click();
  // A service that names its own accounts keeps latchkey's saved
  // browser session: arriving already signed in is one less password,
  // and the login files the credential under whoever that is.
  await expect.poll(() => connectBody?.ephemeral_browser).toBe(false);
  // The config gets who signed in, not the address that was picked,
  // even though the list this stub keeps returning lacks it.
  const toml = await reviewToml(page);
  await expect(toml).toContainText('account = "crusher@enterprise.gov"');
});

test("an existing source reopens on the form that wrote it", async ({ page }) => {
  await pickTile(page, "fastmail", "Copy a Fastmail mailbox over JMAP.");
  await field(page, "Name").fill("Personal mail");
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Personal mail.")).toBeVisible();

  // Not "Email (mbox or other server)": the ingest step's own params say
  // which variant it is, and a preset with no field — on either step —
  // must still count as modeled, or Edit would be disabled on the
  // wizard's own output.
  await expandGroup(page, "personal-mail");
  await pickRowMenu(
    page,
    page.locator(
      '.tg-grid .slick-row:not([data-pinned])[data-key="personal-mail/render_markdown"]',
    ),
    "Edit settings…",
    wizard(page),
  );
  await expect(wizard(page).locator(".wiz-chosen")).toContainText("Fastmail");
});
