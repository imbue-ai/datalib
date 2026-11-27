import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  actOnRowByUuid,
  contextMenuRowByUuid,
  EVERY_ROW,
  GRID,
  openRowByUuid,
  gridSettled,
  inDocFrame,
  MENU_DISABLED,
  menuEntry,
  searchAndSettle,
  searchMenuItem,
  selectRowByUuid,
  shownCards,
  typeInto,
} from "./grid-helpers";

// The contacts app end to end, on a root that has it (`contactsRoot` in
// playwright.config.ts). Riker appears under two handles from two
// sources in the TNG fixture: a Slack user in the Slack source, an email
// address in the Google Takeout chats. A person makes a contact from one
// chip in a document, adds the other handle to it from a chip in
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
    `/applet/unified_grid_and_search_index/search?q=${encodeURIComponent(q)}&limit=50`,
  );
  expect(resp.ok()).toBeTruthy();
  const { rows } = (await resp.json()) as { rows: Row[] };
  const message = rows.find((r) => r.markdown_uuid && r.message_index != null);
  expect(message, `the fixture must have a message by ${handle}`).toBeTruthy();
  await page.goto(EVERY_ROW);
  await searchAndSettle(page, `datalib_document_id:${message!.conversation_uuid}`);
  await gridSettled(page);
  await openRowByUuid(page, message!.uuid);
}

/// The Author chip for `handle` in the open grid.
const gridChip = (page: Page, handle: string) =>
  page.locator(`.grid-box .slick-cell a.chip[data-handle="${handle}"]`).first();

/// The chip for `handle` in the open document, once the document has
/// asked the contacts app about it: unresolved when no contact holds it.
async function chipIn(page: Page, handle: string) {
  return (await inDocFrame(page, `a.chip[data-handle="${handle}"]`)).first();
}

test("two handles from two sources on one contact show it in documents and the grid", async ({
  page,
  request,
}) => {
  test.setTimeout(120_000);
  const card = page.locator(".person");
  const noContact = async (handle: string) => {
    const r = await request.post("/applet/datalib_contacts/resolve", {
      data: { handles: [handle] },
    });
    return (await r.json()) as { resolved: Record<string, unknown> };
  };

  // 1. Riker's Slack message: a click on his chip does nothing, and a
  //    double-click opens his page, where Enter adds him under the name
  //    typed. Opening adds nobody. The grid beside the document, open the
  //    whole time, draws the change without being reloaded.
  await openMessageBy(page, request, SLACK);
  await expect(gridChip(page, SLACK)).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  const slackChip = await chipIn(page, SLACK);
  await expect(slackChip).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  await slackChip.dblclick();
  const add = card.getByRole("button", { name: "Add to contacts" });
  await expect(add).toBeFocused({ timeout: 15_000 });
  expect((await noContact(SLACK)).resolved, "opening the page adds nobody").toEqual({});
  await card.getByLabel("Name for a new contact").fill(CONTACT);
  await page.keyboard.press("Enter");
  await expect(card.locator(".person-about")).toContainText("In your contacts");
  await expect(slackChip).toHaveClass(/handle-resolved/);
  await expect(slackChip).toHaveText(new RegExp(`${CONTACT}$`));
  await expect(gridChip(page, SLACK)).toHaveClass(/handle-resolved/);
  await expect(gridChip(page, SLACK)).toHaveText(new RegExp(`${CONTACT}$`));

  // 2. Riker's Google Chat message, in another source: the email handle
  //    is not anyone's yet, and its page finds the contact to add it to,
  //    reached this time from the chip's menu.
  await openMessageBy(page, request, EMAIL);
  await expect(gridChip(page, EMAIL)).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  const emailChip = await chipIn(page, EMAIL);
  await expect(emailChip).toHaveClass(/handle-unresolved/, { timeout: 15_000 });
  await emailChip.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Add to contacts…" }).click();
  await card.getByLabel("Name for a new contact").fill("Number");
  await card.getByRole("button", { name: `Add to ${CONTACT}` }).click();
  await expect(card.locator(".person-about")).toContainText("In your contacts");
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
    (await resolved.json()) as { resolved: Record<string, { key: string; names: string[] }[]> }
  ).resolved;
  expect(who[SLACK]?.map((c) => c.names[0])).toEqual([CONTACT]);
  expect(who[EMAIL]?.[0]?.key, "both handles belong to the same contact").toBe(
    who[SLACK]?.[0]?.key,
  );

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
    data: { contact_id: who[SLACK]?.[0]?.key, name: "Will Riker" },
  });
  expect(renamed.ok()).toBeTruthy();
  await expect(gridChip(page, EMAIL)).toHaveText(/Will Riker$/, { timeout: 15_000 });
  await expect(await chipIn(page, EMAIL)).toHaveText(/Will Riker$/);
});

/// A handle taken off a contact from its menu, and one marked as no longer
/// working, whose date is then typed over. A click on the chip does
/// neither. Made-up handles, which no other test touches.
test("a handle's menu on its contact's page takes it off and marks it as old", async ({
  page,
  request,
}) => {
  const n = Date.now();
  const home = `email:wesley.${n}@earth.test`;
  const school = `email:crusher.${n}@academy.test`;
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Wesley", handles: [home, school] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();
  const holders = async () => {
    const r = await request.post("/applet/datalib_contacts/resolve", {
      data: { handles: [home, school] },
    });
    type Who = { handles: { handle: string; stopped_working_by: string | null }[] };
    const { resolved } = (await r.json()) as { resolved: Record<string, Who[]> };
    return Object.fromEntries(Object.entries(resolved).map(([h, [c]]) => [h, c!]));
  };

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(home)}, {})`)}`);
  const card = page.locator(".person");
  const chip = (h: string) => card.locator(`.person-mine a.chip[data-handle="${h}"]`);
  await expect(chip(school)).toHaveClass(/handle-resolved/, { timeout: 15_000 });

  await chip(school).click();
  await chip(school).click({ button: "right" });
  await page.getByRole("menuitem", { name: "No longer works" }).click();
  const by = card.getByTitle("2019, 2019-06 or 2019-06-14; empty when it works again");
  await expect(by).toHaveValue(/^\d{4}-\d{2}-\d{2}$/);
  await by.fill("2364");
  await by.press("Enter");
  await expect
    .poll(async () => (await holders())[home]?.handles.find((h) => h.handle === school))
    .toMatchObject({ stopped_working_by: "2364" });

  await chip(school).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Not theirs" }).click();
  await expect(chip(school)).toHaveCount(0);
  await expect.poll(async () => Object.keys(await holders())).toEqual([home]);
  const down = (await (await request.get("/applet/datalib_contacts/not_linked")).json()) as {
    not_linked: { handle: string; agent: string | null }[];
  };
  expect(down.not_linked).toContainEqual(expect.objectContaining({ handle: school, agent: null }));
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
      `/applet/unified_grid_and_search_index/search?q=${encodeURIComponent(q)}&limit=1`,
    );
    expect(r.ok()).toBeTruthy();
    const body = (await r.json()) as { total: number; refused?: string[] };
    expect(body.refused ?? []).toEqual([]);
    return body.total;
  };
  const byHandle = await total(`with:${picard}`);
  expect(byHandle, "the fixture names Picard's address").toBeGreaterThan(0);

  await page.goto(GRID);
  const field = page.getByTestId("search-input");
  await typeInto(field, "@jean");
  const offered = page.locator(`.cm-tooltip-autocomplete a.chip[data-contact="${id}"]`);
  await expect(offered).toBeVisible();
  await expect(offered).toContainText("Jean-Luc");
  await field.press("Tab");
  await expect(field).toHaveAttribute("data-query", `with:contact:${id} `);
  await expect(field.locator(`a.chip[data-contact="${id}"]`)).toContainText("Jean-Luc");
  expect(await total(`with:contact:${id}`)).toBe(byHandle);
});

/// "This is me" on the person card: the contacts table marks the contact,
/// and `from:me` finds what its address wrote, which no source says is
/// the owner's. Data's address, which no other test touches; unmarked at
/// the end, since the root is shared.
test("a contact marked as me on its card is who from:me finds", async ({ page, request }) => {
  const data = "email:data@enterprise.starfleet";
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Mr. Data", handles: [data] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();
  const { contact_id: id } = (await made.json()) as { contact_id: string };
  const total = async (q: string) => {
    const r = await request.get(
      `/applet/unified_grid_and_search_index/search?q=${encodeURIComponent(q)}&limit=1`,
    );
    expect(r.ok()).toBeTruthy();
    const body = (await r.json()) as { total: number; refused?: string[] };
    expect(body.refused ?? []).toEqual([]);
    return body.total;
  };
  const his = await total(`from:${data}`);
  expect(his, "the fixture has messages from Data").toBeGreaterThan(0);
  expect(await total(`from:me from:${data}`)).toBe(0);

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(data)}, {})`)}`);
  const card = page.locator(".person");
  const isMe = card.getByTestId("contact-is-me");
  await expect(isMe).not.toBeChecked({ timeout: 15_000 });
  await isMe.check();
  await expect.poll(() => total(`from:me from:${data}`)).toBe(his);

  await page.goto("/contactsView()");
  const table = page.locator(".contacts-card");
  await table.getByLabel("Filter contacts").fill("Mr. Data");
  await expect(table.locator(`.slick-row[data-key="${id}"] [col-id="me"]`)).toContainText("me");

  const off = await request.post("/applet/datalib_contacts/me", {
    data: { contact_id: id, me: false },
  });
  expect(off.ok()).toBeTruthy();
  expect(await total(`from:me from:${data}`)).toBe(0);
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
  await card.getByLabel("Add a detail").selectOption("title");
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
/// offers to be added, and the saved field remembers where it came from.
test("a source's values copy into the contact, and a copied address is added in one click", async ({
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

  // Opening the draft takes a moment, and the next click can come first:
  // that second copy must not start the form over and lose the first.
  let letDraftOpen!: () => void;
  const draftMayOpen = new Promise<void>((open) => (letDraftOpen = open));
  await page.route("**/applet/datalib_contacts/contact/*/draft", async (r) => {
    if (r.request().method() === "POST") await draftMayOpen;
    await r.fallback();
  });

  // The address book's record holds his second address too.
  const book = card.locator(".person-section", { hasText: "laforge@enterprise.starfleet" }).last();
  await book.locator(".person-copy summary").click();
  await book.getByRole("button", { name: "Use Title “Chief Engineer”" }).click();
  await book
    .getByRole("button", { name: "Use Email (work) “laforge@enterprise.starfleet”" })
    .click();
  letDraftOpen();
  await expect(card.getByLabel("Title", { exact: true })).toHaveValue("Chief Engineer");
  await expect(book.locator(".person-copy-values")).toContainText(
    "Title “Chief Engineer”in your contact",
  );

  // The copied address is on nobody; one click adds it here.
  await card.getByRole("button", { name: "Add it", exact: true }).click();
  await expect
    .poll(async () => {
      const r = await request.post("/applet/datalib_contacts/resolve", {
        data: { handles: [other] },
      });
      return ((await r.json()) as { resolved: Record<string, { key: string }[]> }).resolved[
        other
      ]?.[0]?.key;
    })
    .toBe(id);
  await expect(card.getByRole("button", { name: "Add it", exact: true })).toHaveCount(0);

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

type Held = { id: string; name: string; handles: (string | null)[] };

/// Every contact each of `handles` is on, by handle, by name.
async function holdersOf(request: APIRequestContext, handles: string[]) {
  const r = await request.post("/applet/datalib_contacts/resolve", { data: { handles } });
  expect(r.ok()).toBeTruthy();
  type Contact = { key: string; names: string[]; handles: { handle: string | null }[] };
  const { resolved } = (await r.json()) as { resolved: Record<string, Contact[]> };
  return Object.fromEntries(
    Object.entries(resolved).map(([h, cs]) => [
      h,
      cs.map((c): Held => ({
        id: c.key,
        name: c.names[0]!,
        handles: c.handles.map((x) => x.handle).sort(),
      })),
    ]),
  );
}

/// The contact each of `handles` is on, by handle, where it is on one.
async function heldWith(request: APIRequestContext, handles: string[]) {
  const all = await holdersOf(request, handles);
  return Object.fromEntries(
    Object.entries(all).flatMap(([h, cs]) => (cs.length === 1 ? [[h, cs[0]!]] : [])),
  ) as Record<string, Held>;
}

async function deleteContacts(request: APIRequestContext, ids: string[]) {
  for (const id of new Set(ids)) {
    const r = await request.delete(`/applet/datalib_contacts/contact/${id}`);
    expect(r.ok(), await r.text()).toBeTruthy();
  }
}

/// Worf's address-book card lists an address and a number. Opened from the
/// number, his page offers both, ticked; what is unticked is left out, and
/// the page then offers the rest of what the card says.
test("adding someone takes every ticked handle a source says is theirs", async ({
  page,
  request,
}) => {
  const tel = "tel:+12025550102";
  const address = "email:worf@enterprise.starfleet";
  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(tel)}, {})`)}`);
  const card = page.locator(".person");
  const offers = card.getByRole("list", { name: "Handles to add" });
  const box = (shown: string) => offers.getByRole("checkbox", { name: shown });
  await expect(box("+12025550102")).toBeChecked({ timeout: 15_000 });
  await expect(box("worf@enterprise.starfleet")).toBeChecked();

  await box("worf@enterprise.starfleet").uncheck();
  await card.getByRole("button", { name: "Add to contacts" }).click();
  await expect(card.locator(".person-about")).toContainText("In your contacts");
  await expect(offers).toHaveCount(0);
  let held = await heldWith(request, [tel, address]);
  expect(held[tel]?.handles).toEqual([tel]);
  expect(held[address]).toBeUndefined();

  await card
    .getByRole("button", { name: /^Add its other handle/ })
    .first()
    .click();
  await expect
    .poll(async () => (await heldWith(request, [address]))[address]?.handles)
    .toContain(tel);
  held = await heldWith(request, [tel]);
  await deleteContacts(request, [held[tel].id]);
});

/// Sonya Gomez's card holds one number, written without its country code, so
/// it has no handle. Its heading still opens her page, which offers the full
/// number a Google Voice caller has, and the caller's page shows her card.
test("a card with no handle opens its person, by the number with the same digits", async ({
  page,
  request,
}) => {
  const full = "tel:+12025550147";
  const r = await request.get(
    `/applet/unified_grid_and_search_index/search?q=${encodeURIComponent("with:(202)555-0147 source:tng_contacts")}&limit=5`,
  );
  const { rows } = (await r.json()) as { rows: Row[] };
  expect(rows.length, "the fixture has Sonya's card").toBe(1);
  await page.goto(`/${encodeURIComponent(`documentView(${JSON.stringify(rows[0].uuid)})`)}`);
  const heading = (await inDocFrame(page, "h1 a.chip[data-contact-page]")).first();
  await expect(heading).toHaveText(/Sonya Gomez$/);
  await heading.dblclick();

  const card = page.locator(".person");
  const offered = card.getByRole("checkbox", { name: "+12025550147" });
  await expect(offered).toBeChecked({ timeout: 15_000 });
  await expect(card.locator(".person-offers")).toContainText("written (202) 555-0147");
  await expect(card.getByRole("button", { name: "Add to contacts" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(card.locator(".person-about")).toContainText("In your contacts");
  const held = await heldWith(request, [full]);
  expect(held[full]).toMatchObject({ name: "Sonya Gomez", handles: [full] });
  await expect(heading).toHaveClass(/handle-resolved/);
  await deleteContacts(request, [held[full].id]);

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(full)}, {})`)}`);
  const book = card.locator(".person-section", { hasText: "same digits" }).last();
  await expect(book).toContainText("Sonya Gomez", { timeout: 15_000 });
  await expect(card.getByLabel("Name for a new contact")).toHaveValue("Sonya Gomez");
});

/// Several rows of an address book become contacts in one go, a group among
/// them left out and named, and adding them again adds nobody.
test("the selected cards of an address book are added to contacts", async ({ page, request }) => {
  const cards = {
    "Seven of Nine": "email:seven.of.nine@unimatrix01.borg",
    Hugh: "email:third.of.five@unimatrix01.borg",
  };
  const listed = await request.get(
    `/applet/unified_grid_and_search_index/search?q=${encodeURIComponent("source:tng_contacts is:document")}&limit=50`,
  );
  type CardRow = { uuid: string; contact: string | null };
  const { rows } = (await listed.json()) as { rows: CardRow[] };
  const uuid = (name: string) => {
    const row = rows.find((x) => x.contact === name);
    expect(row, `the fixture has ${name}`).toBeTruthy();
    return row!.uuid;
  };

  const toasts = page.locator(".datalib-toast");
  const selectAndAdd = async () => {
    await page.goto(GRID);
    await searchAndSettle(page, "source:tng_contacts");
    await gridSettled(page);
    await selectRowByUuid(page, uuid("Seven of Nine"));
    for (const name of ["Hugh", "Senior Staff"]) {
      await actOnRowByUuid(page, uuid(name), (row) =>
        row.click({ modifiers: ["ControlOrMeta"], position: { x: 40, y: 10 }, timeout: 3_000 }),
      );
    }
    await contextMenuRowByUuid(page, uuid("Hugh"));
    await searchMenuItem(page, "Add to contacts (3)").click();
  };
  await selectAndAdd();
  await expect(
    toasts.filter({
      hasText: "Added 2 people to contacts; Senior Staff skipped: a group is not a person",
    }),
  ).toBeVisible();
  const held = await heldWith(request, Object.values(cards));
  for (const [name, handle] of Object.entries(cards)) expect(held[handle]?.name).toBe(name);
  expect(held[cards["Seven of Nine"]].handles, "every handle on the card").toContain(
    "email:annika.hansen@raven.fed",
  );

  await selectAndAdd();
  await expect(toasts.filter({ hasText: "2 already there" })).toBeVisible();
  await deleteContacts(
    request,
    Object.values(held).map((c) => c.id),
  );
});

/// A contact with no handle has a page like any other: its row opens it, and
/// so does its chip.
test("a contact with no handle has a page", async ({ page, request }) => {
  const name = `Mot ${Date.now()}`;
  const made = await request.post("/applet/datalib_contacts/contacts", { data: { name } });
  expect(made.ok(), await made.text()).toBeTruthy();
  const { contact_id: id } = (await made.json()) as { contact_id: string };
  await page.goto("/contactsView()");
  const list = page.locator(".contacts-card");
  await list.getByLabel("Filter contacts").fill(name);
  await list.locator(`.slick-row[data-key="${id}"] a.chip[data-contact="${id}"]`).dblclick();
  const card = page.locator(".person");
  await expect(card.locator(".person-name")).toHaveText(new RegExp(`${name}$`), {
    timeout: 15_000,
  });
  await expect(card.locator(".person-about")).toContainText("In your contacts");
  await card.getByRole("button", { name: "Edit", exact: true }).click();
  await card.getByLabel("Name", { exact: true }).fill(`${name} the barber`);
  await card.getByRole("button", { name: "Save", exact: true }).click();
  await expect(card.locator(".person-name")).toHaveText(/the barber$/);

  await deleteContacts(request, [id]);
  await expect(card).toContainText("This contact is no longer in your contacts.");
});

/// Robert and Marie Picard share the vineyard's address. With Robert in
/// your contacts, Marie's page offers it unticked as his; ticked, both
/// have it. Its chip then names them both, and its page is its own, with
/// each of them on it.
test("an address two people share is on both, and its chip names them", async ({
  page,
  request,
}) => {
  const vineyard = "email:vineyard@labarre.earth";
  const marie = "email:marie@labarre.earth";
  const made = await request.post("/applet/datalib_contacts/contacts", {
    data: { name: "Robert Picard", handles: [vineyard] },
  });
  expect(made.ok(), await made.text()).toBeTruthy();

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(marie)}, {})`)}`);
  const card = page.locator(".person");
  const offers = card.getByRole("list", { name: "Handles to add" });
  const shared = offers.getByRole("checkbox", { name: "vineyard@labarre.earth" });
  await expect(offers.getByRole("checkbox", { name: "marie@labarre.earth" })).toBeChecked({
    timeout: 15_000,
  });
  await expect(shared).not.toBeChecked();
  await expect(offers).toContainText("on Robert Picard");
  await shared.check();
  await expect(offers.getByRole("radio", { name: "Both" })).toBeChecked();
  await expect(card.getByLabel("Name for a new contact")).toHaveValue("Marie Picard");
  await card.getByRole("button", { name: "Add to contacts" }).click();
  await expect(card.locator(".person-about")).toContainText("In your contacts");
  await expect(card.locator(".person-mine")).toContainText("shared with Robert Picard");

  const holders = await holdersOf(request, [vineyard, marie]);
  expect(holders[vineyard]?.map((c) => c.name)).toEqual(["Marie Picard", "Robert Picard"]);
  expect(holders[marie]?.map((c) => c.name)).toEqual(["Marie Picard"]);

  await page.goto(`/${encodeURIComponent(`personView(${JSON.stringify(vineyard)}, {})`)}`);
  const heading = card.locator(".person-name a.chip");
  await expect(heading).toHaveText(/Marie Picard & Robert Picard$/, { timeout: 15_000 });
  await expect(heading).toHaveClass(/handle-resolved/);
  await expect(heading).toHaveAttribute("title", /Shared by Marie Picard, Robert Picard/);
  await expect(card.locator(".person-about")).toContainText("Shared by 2 of your contacts");
  const who = card.getByRole("list", { name: "Who has it" });
  await expect(who.locator("a.chip[data-contact]")).toHaveText([/Marie Picard$/, /Robert Picard$/]);
  await expect(card.getByRole("button", { name: "Add to contacts" })).toHaveCount(0);

  await deleteContacts(
    request,
    (holders[vineyard] ?? []).map((c) => c.id),
  );
});
