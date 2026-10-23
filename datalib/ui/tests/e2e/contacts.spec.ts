import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  EVERY_ROW,
  GRID,
  selectRowByUuid,
  gridSettled,
  inDocFrame,
  MENU_DISABLED,
  menuEntry,
  searchAndSettle,
  shownCards,
  typeInto,
} from "./grid-helpers";

// The contacts app end to end, on a root that has it (`contactsRoot` in
// playwright.config.ts). Riker appears under two handles from two
// sources in the TNG fixture: a Slack user in the Slack source, an email
// address in the Google Takeout chats. A person makes a contact from one
// chip in a document, links the other handle to it from a chip in
// another source's document, and from then on both chips, in documents
// and in the grid's Author column, show the contact rather than what
// either source called him (docs/dev/chips.md). An edit made outside
// the page reaches the chips it already drew.

const SLACK = "slack:T_NCC1701D/U_RIKER";
const EMAIL = "email:riker@enterprise.starfleet";
// Neither source's name for him, so a chip showing it was resolved
// through the contact and not drawn from the source.
const CONTACT = "Number One";

type Row = {
  uuid: string;
  conversation_uuid: string;
  markdown_uuid: string | null;
  message_index: number | null;
};

/// A message Riker wrote under `handle`, its document opened from a grid
/// of its conversation, which stays open beside it. The conversation and
/// not his rows alone: the grid hides a column whose values are all the
/// same, so a grid of one author has no Author column to look at.
async function openMessageBy(page: Page, request: APIRequestContext, handle: string) {
  const q = `author_handle:"${handle}"`;
  const resp = await request.get(
    `/applet/unified_document_index/search?q=${encodeURIComponent(q)}&limit=50`,
  );
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const message = rows.find((r) => r.markdown_uuid && r.message_index != null);
  expect(message, `the fixture must have a message by ${handle}`).toBeTruthy();
  await page.goto(EVERY_ROW);
  await searchAndSettle(page, `convo:${message!.conversation_uuid}`);
  await gridSettled(page);
  await selectRowByUuid(page, message!.uuid);
}

/// The Author chip for `handle` in the open grid.
const gridChip = (page: Page, handle: string) =>
  page.locator(`.grid-box .slick-cell a.chip[data-handle="${handle}"]`).first();

/// The chip for `handle` in the open document, once the document has
/// asked the contacts app about it: linkable when no contact holds it.
async function chipIn(page: Page, handle: string) {
  return (await inDocFrame(page, `a.chip[data-handle="${handle}"]`)).first();
}

test("two handles from two sources linked to one contact show it in documents and the grid", async ({
  page,
  request,
}) => {
  test.setTimeout(120_000);
  const popover = page.locator(".handle-popover");

  // 1. Riker's Slack message: the chip offers a link, and a new contact
  //    takes the Slack handle. The grid beside the document, open the
  //    whole time, draws the link without being reloaded.
  await openMessageBy(page, request, SLACK);
  await expect(gridChip(page, SLACK)).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  const slackChip = await chipIn(page, SLACK);
  await expect(slackChip).toHaveClass(/handle-linkable/, { timeout: 15_000 });
  await slackChip.click();
  await expect(popover).toBeVisible();
  await popover.getByLabel("Contact name").fill(CONTACT);
  await popover.getByRole("button", { name: `New contact “${CONTACT}”` }).click();
  await expect(popover).toBeHidden();
  await expect(slackChip).toHaveClass(/handle-resolved/);
  await expect(slackChip).toHaveText(new RegExp(`${CONTACT}$`));
  await expect(gridChip(page, SLACK)).toHaveClass(/handle-resolved/);
  await expect(gridChip(page, SLACK)).toHaveText(new RegExp(`${CONTACT}$`));

  // 2. Riker's Google Chat message, in another source: the email handle
  //    is not anyone's yet, and the popover finds the contact to link it to.
  await openMessageBy(page, request, EMAIL);
  await expect(gridChip(page, EMAIL)).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  const emailChip = await chipIn(page, EMAIL);
  await expect(emailChip).toHaveClass(/handle-linkable/, { timeout: 15_000 });
  await emailChip.click();
  await expect(popover).toBeVisible();
  await popover.getByLabel("Contact name").fill("Number");
  await popover.getByRole("button", { name: `Link to ${CONTACT}` }).click();
  await expect(popover).toBeHidden();
  await expect(emailChip).toHaveClass(/handle-resolved/);
  await expect(emailChip).toHaveText(new RegExp(`${CONTACT}$`));
  await expect(gridChip(page, EMAIL)).toHaveClass(/handle-resolved/);
  await expect(gridChip(page, EMAIL)).toHaveText(new RegExp(`${CONTACT}$`));

  // 3. The store says both handles are the one contact.
  const resolved = await request.post("/applet/datalib_contacts/resolve", {
    data: { handles: [SLACK, EMAIL] },
  });
  expect(resolved.ok()).toBeTruthy();
  const who = (
    (await resolved.json()) as { resolved: Record<string, { key: string; names: string[] }> }
  ).resolved;
  expect(who[SLACK]?.names[0]).toBe(CONTACT);
  expect(who[EMAIL]?.key, "both handles belong to the same contact").toBe(who[SLACK]?.key);

  // 4. A page loaded afresh asks the store again and shows the same.
  for (const handle of [SLACK, EMAIL]) {
    await openMessageBy(page, request, handle);
    await expect(gridChip(page, handle)).toHaveClass(/handle-resolved/, { timeout: 15_000 });
    await expect(gridChip(page, handle)).toHaveText(new RegExp(`${CONTACT}$`));
    await expect(await chipIn(page, handle)).toHaveText(new RegExp(`${CONTACT}$`));
  }

  // 5. An edit made outside this page — another window, an agent — reaches
  //    the chips already drawn, without a reload: the server sees the
  //    contacts store publish and tells every page to ask again.
  const renamed = await request.post("/applet/datalib_contacts/rename", {
    data: { contact_id: who[SLACK]?.key, name: "Will Riker" },
  });
  expect(renamed.ok()).toBeTruthy();
  await expect(gridChip(page, EMAIL)).toHaveText(/Will Riker$/, { timeout: 15_000 });
  await expect(await chipIn(page, EMAIL)).toHaveText(/Will Riker$/);
});

/// One of your contacts in the search bar: `@` and a few letters of its
/// name offer it, picking it writes `with:contact:<id>` drawn as the
/// contact's chip, and the search finds what reached any handle linked
/// to it. Picard's address, which the other test leaves alone.
test("a contact picked with @ finds everything that reached its handles", async ({
  page,
  request,
}) => {
  const picard = "email:picard@enterprise.starfleet";
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Jean-Luc", handles: [picard] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();
  const { contact_id: id } = (await made.json()) as { contact_id: string };

  const total = async (q: string) => {
    const r = await request.get(
      `/applet/unified_document_index/search?q=${encodeURIComponent(q)}&limit=1`,
    );
    expect(r.ok()).toBeTruthy();
    const body = (await r.json()) as { total: number; refused?: string[] };
    expect(body.refused ?? []).toEqual([]);
    return body.total;
  };
  const byHandle = await total(`with:${picard}`);
  expect(byHandle, "the fixture names Picard's address").toBeGreaterThan(0);

  await page.goto(GRID);
  const field = shownCards(page).getByTestId("search-input");
  await typeInto(field, "@jean");
  const offered = shownCards(page).locator(`.cm-tooltip-autocomplete a.chip[data-contact="${id}"]`);
  await expect(offered).toBeVisible();
  await expect(offered).toContainText("Jean-Luc");
  await field.press("Tab");
  await expect(field).toHaveAttribute("data-query", `with:contact:${id} `);
  await expect(field.locator(`a.chip[data-contact="${id}"]`)).toContainText("Jean-Luc");
  expect(await total(`with:contact:${id}`)).toBe(byHandle);
});

/// Two contacts made for one person by mistake: the one with no handle is
/// still a chip, selecting both and right-clicking the one to keep merges
/// the other into it, and Delete takes the survivor away.
test("the contacts table draws a contact with no handle as a chip, merges and deletes", async ({
  page,
  request,
}) => {
  const name = `Guinan ${Date.now()}`;
  const make = async (handles: string[]) => {
    const r = await request.post("/applet/datalib_contacts/contacts", {
      data: { name, handles },
    });
    expect(r.ok(), await r.text()).toBeTruthy();
    return ((await r.json()) as { contact_id: string }).contact_id;
  };
  const keep = await make(["email:guinan@ten-forward.test"]);
  const dup = await make([]);

  await page.goto("/contactsView()");
  const card = page.locator(".contacts-card");
  await card.getByLabel("Filter contacts").fill(name);
  const row = (id: string) => card.locator(`.slick-row[data-key="${id}"]`);
  await expect(row(dup).locator(`a.chip[data-contact="${dup}"]`)).toContainText(name);
  await expect(row(keep).locator("a.chip[data-handle]")).toContainText(name);

  await row(dup).locator('[col-id="kind"]').click();
  await row(keep).locator('[col-id="kind"]').click({ button: "right" });
  await expect(menuEntry(page, /^Merge into this one/)).toHaveClass(MENU_DISABLED);
  await page.keyboard.press("Escape");
  await row(dup).locator('[col-id="kind"]').click();
  await row(keep)
    .locator('[col-id="kind"]')
    .click({ modifiers: ["ControlOrMeta"] });
  await row(keep).locator('[col-id="kind"]').click({ button: "right" });
  await menuEntry(page, "Merge the other selected contact into this one").click();
  await expect(row(dup)).toHaveCount(0);
  await expect(row(keep)).toHaveCount(1);
  const merged = await request.get(`/applet/datalib_contacts/contact/${dup}`);
  expect(((await merged.json()) as { key: string }).key, "the old id answers as the survivor").toBe(
    keep,
  );

  page.once("dialog", (d) => void d.accept());
  await row(keep).locator('[col-id="kind"]').click({ button: "right" });
  await menuEntry(page, "Delete contact…").click();
  await expect(row(keep)).toHaveCount(0);
  const gone = await request.get(`/applet/datalib_contacts/contact/${keep}`);
  expect(gone.status()).toBe(404);
});

/// Editing your contact on its person card: the edit is a draft nobody
/// else sees, which a reload resumes; a rename made elsewhere just
/// before Save makes the save refuse rather than overwrite it, and
/// saving again keeps the card's name. The photo changes at once.
test("the person card edits a contact through a draft and refuses a stale save", async ({
  page,
  request,
}) => {
  test.setTimeout(90_000);
  const handle = `email:counselor.${Date.now()}@betazed.test`;
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Deanna", handles: [handle] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();
  const { contact_id: id } = (await made.json()) as { contact_id: string };
  const api = `/applet/datalib_contacts/contact/${id}`;
  type Edit = { name: string; note: string | null; fields: { kind: string; value: string }[] };
  const published = async () => (await (await request.get(`${api}/edit`)).json()) as Edit;

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(handle)}, {})`)}`);
  const card = page.locator(".person");
  await expect(card.locator(".person-name")).toHaveText(/Deanna$/, { timeout: 15_000 });

  // 1. The edit autosaves to the draft, and nothing is published.
  await card.getByRole("button", { name: "Edit", exact: true }).click();
  await card.getByLabel("Name", { exact: true }).fill("Counselor Troi");
  await card.getByLabel("Add a field").selectOption("title");
  await card.getByLabel("Title", { exact: true }).fill("Ship's counselor");
  await card.getByLabel("Note", { exact: true }).fill("Chocolate.");
  await expect
    .poll(async () => {
      const r = await request.get(`${api}/draft`);
      return r.ok() ? ((await r.json()) as { mine: Edit }).mine : null;
    })
    .toMatchObject({
      name: "Counselor Troi",
      note: "Chocolate.",
      fields: [{ kind: "title", value: "Ship's counselor" }],
    });
  expect((await published()).name).toBe("Deanna");

  // 2. A reload resumes the draft.
  await page.reload();
  await expect(card.getByLabel("Name", { exact: true })).toHaveValue("Counselor Troi", {
    timeout: 15_000,
  });

  // 3. Renamed elsewhere after the card last looked: the save sees it
  //    first, so it is refused, and the card marks the name.
  await page.route(
    "**/draft/save",
    async (route) => {
      const renamed = await request.post("/applet/datalib_contacts/rename", {
        data: { contact_id: id, name: "Deanna Troi-Riker" },
      });
      expect(renamed.ok()).toBeTruthy();
      await route.continue();
    },
    { times: 1 },
  );
  await card.getByRole("button", { name: "Save", exact: true }).click();
  await expect(card.getByRole("alert")).toContainText("changed elsewhere");
  await expect(card.locator(".contact-edit-conflict")).toContainText(
    "The name was changed elsewhere to “Deanna Troi-Riker”.",
  );
  expect((await published()).name).toBe("Deanna Troi-Riker");

  // 4. Keeping mine and saving again publishes the card's edit.
  await card.getByRole("button", { name: "Keep mine" }).click();
  await card.getByRole("button", { name: "Save", exact: true }).click();
  await expect(card.getByRole("button", { name: "Edit", exact: true })).toBeVisible();
  await expect(card.locator(".person-name")).toHaveText(/Counselor Troi$/);
  await expect(card.locator(".person-lines")).toContainText("Ship's counselor");
  expect(await published()).toMatchObject({
    name: "Counselor Troi",
    note: "Chocolate.",
    fields: [{ kind: "title", value: "Ship's counselor" }],
  });
  expect((await request.get(`${api}/draft`)).status(), "the save drops the draft").toBe(404);

  // 5. A photo is set from a file and removed, each at once.
  await card.getByLabel("Photo file").setInputFiles({
    name: "troi.png",
    mimeType: "image/png",
    buffer: Buffer.from(PNG_1PX, "base64"),
  });
  await expect(card.locator(".person-head-photo")).toHaveAttribute(
    "src",
    new RegExp(`/applet/datalib_contacts/photo/${id}\\?v=\\d+$`),
  );
  const first = await card.locator(".person-head-photo").getAttribute("src");

  // An image file dropped on the photo replaces it.
  const drop = await page.evaluateHandle((b64) => {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const dt = new DataTransfer();
    dt.items.add(new File([bytes], "deanna.png", { type: "image/png" }));
    return dt;
  }, PNG_1PX);
  await card.locator(".contact-edit-drop").dispatchEvent("drop", { dataTransfer: drop });
  await expect(card.locator(".person-head-photo")).not.toHaveAttribute("src", first!);
  await expect(card.locator(".person-head-photo")).toHaveAttribute(
    "src",
    new RegExp(`/applet/datalib_contacts/photo/${id}\\?v=\\d+$`),
  );

  await card.getByRole("button", { name: "Remove photo" }).click();
  await expect(card.locator(".person-head-photo")).toHaveCount(0);
});

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const Buffer: { from(data: string, encoding: "base64"): Uint8Array };

/// A one-pixel PNG, the smallest photo the contacts app takes.
const PNG_1PX =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// Copying what a source says into your contact: the address book's
/// record of Geordi offers each value, a copied address that nobody holds
/// offers to be linked, and the saved field remembers where it came from.
test("a source's values copy into the contact, and a copied address links in one click", async ({
  page,
  request,
}) => {
  test.setTimeout(90_000);
  const handle = "email:glaforge@enterprise.starfleet";
  const other = "email:laforge@enterprise.starfleet";
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Geordi", handles: [handle] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();
  const { contact_id: id } = (await made.json()) as { contact_id: string };

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(handle)}, {})`)}`);
  const card = page.locator(".person");
  await expect(card.locator(".person-name")).toHaveText(/Geordi$/, { timeout: 15_000 });

  // The address book's record holds his second address too.
  const book = card.locator(".person-section", { hasText: "laforge@enterprise.starfleet" }).last();
  await book.locator(".person-copy summary").click();
  await book.getByRole("button", { name: "Copy Title “Chief Engineer”" }).click();
  await book
    .getByRole("button", { name: "Copy Email (work) “laforge@enterprise.starfleet”" })
    .click();
  await expect(card.getByLabel("Title", { exact: true })).toHaveValue("Chief Engineer");
  await expect(book.locator(".person-copy-values")).toContainText(
    "Title “Chief Engineer”in your contact",
  );

  // The copied address is linked to nobody; one click links it here.
  await card.getByRole("button", { name: "Link it" }).click();
  await expect
    .poll(async () => {
      const r = await request.post("/applet/datalib_contacts/resolve", {
        data: { handles: [other] },
      });
      return ((await r.json()) as { resolved: Record<string, { key: string }> }).resolved[other]
        ?.key;
    })
    .toBe(id);
  await expect(card.getByRole("button", { name: "Link it" })).toHaveCount(0);

  await card.getByRole("button", { name: "Save", exact: true }).click();
  await expect(card.getByRole("button", { name: "Edit", exact: true })).toBeVisible();
  const published = (await (
    await request.get(`/applet/datalib_contacts/contact/${id}/edit`)
  ).json()) as { fields: { kind: string; value: string; copied_from: unknown }[] };
  expect(published.fields).toEqual([
    expect.objectContaining({
      kind: "title",
      value: "Chief Engineer",
      copied_from: expect.objectContaining({ source_id: expect.any(String) }),
    }),
    expect.objectContaining({ kind: "email", value: "laforge@enterprise.starfleet" }),
  ]);
});
