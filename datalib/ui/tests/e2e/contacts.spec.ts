import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  EVERY_ROW,
  GRID,
  selectRowByUuid,
  gridSettled,
  inDocFrame,
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
    `/applet/unified_index/search?q=${encodeURIComponent(q)}&limit=50`,
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
    const r = await request.get(`/applet/unified_index/search?q=${encodeURIComponent(q)}&limit=1`);
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
