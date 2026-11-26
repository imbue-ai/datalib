// Whoever one card in an address book is about, by the card's own page: what
// names a person whose card has no handle (docs/dev/chips.md § "A contact's
// page"). The look is a person's who is not in your contacts.

import { writeContactPage } from "./chipLinks";
import { pageContact, recordHandles, sureNumbers } from "./person";
import {
  addPeople,
  contactLook,
  drawChip,
  nameOf,
  noteContactsApp,
  numbersOf,
  people,
  peopleFor,
  resolveHandles,
  type AddedPeople,
  type ChipLook,
  type FullNumbers,
  type NewPerson,
  type NormalizedContact,
} from "./contacts";
import { Resolver } from "./resolver";
import { pushToast } from "@/toasts";

/** A card, and the full numbers its numbers with no country code may be. */
export type PageAnswer = { contact: NormalizedContact; fullNumbers: FullNumbers };

/** Each card by its page's id, for the whole app; a page that is not a
 *  contact's is absent. */
export const contactPages = new Resolver<PageAnswer>(
  async (pages) => {
    const [mine, answer] = await Promise.all([resolveHandles([]), peopleFor([], pages)]);
    noteContactsApp(mine !== null);
    const out = new Map(
      Object.entries(answer.pages).map(([page, contact]) => [
        page,
        { contact, fullNumbers: numbersOf([contact], answer.full_numbers) },
      ]),
    );
    // Whose contact each is, which its chip says, is who has its number.
    await people.ask([...out.values()].flatMap((a) => sureNumbers(a.contact, a.fullNumbers)));
    return out;
  },
  (e) => pushToast(`Contacts: ${e.message}`),
);

/** Whether an answer about some handles can have changed what a page's chip
 *  shows: they are the numbers on a card here. */
export function movesPages(handles: ReadonlySet<string>, pages: string[]): boolean {
  return pages.some((p) => {
    const a = contactPages.get(p);
    return !!a && sureNumbers(a.contact, a.fullNumbers).some((h) => handles.has(h));
  });
}

/** How the chip looks: your contact, where the card's number is on one, else
 *  the card's own name once it is read. Until then, `shown`, which is empty
 *  where whoever wrote the link wrote that too. */
export function contactPageLook(shown: string, answer: PageAnswer | undefined): ChipLook {
  const mine = pageContact(answer, (h) => people.get(h));
  if (mine) return contactLook(nameOf(mine), mine);
  const text = answer ? nameOf(answer.contact) : shown || "a contact";
  const numbers = (answer?.contact.handles ?? []).map((h) => h.value);
  return {
    text,
    ariaLabel: `${text}, not in your contacts`,
    classes: ["handle-chip", "handle-unresolved"],
    initial: null,
    icon: "contacts",
    photo: answer?.contact.photo_url ?? null,
    title: [text, ...numbers, "Double-click to open"].join("\n"),
  };
}

/** The chip for a grid cell or a heading. */
export function contactPageCell(
  page: string,
  shown: string,
  answer: PageAnswer | undefined,
): HTMLAnchorElement {
  const a = document.createElement("a");
  a.className = "chip";
  a.href = writeContactPage(page);
  a.dataset.contactPage = page;
  a.dataset.shownAs = shown;
  drawChip(a, contactPageLook(shown, answer));
  return a;
}

/** Draw every contact page's chip under `root`: a document's decorate pass.
 *  A link anyone can write shows the card it names and never its own text. */
export async function decorateContactPages(root: HTMLElement): Promise<void> {
  const chips = Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-contact-page]"));
  if (chips.length === 0) return;
  await contactPages.ask(new Set(chips.map((c) => c.dataset.contactPage ?? "")));
  for (const c of chips) {
    if (!c.isConnected) continue;
    const answer = contactPages.get(c.dataset.contactPage ?? "");
    const look = contactPageLook("", answer);
    c.dataset.shownAs = look.text;
    drawChip(c, look);
  }
}

/** Add everyone these cards are about to your contacts, each with every
 *  handle their card says is theirs. A group is nobody to add. */
export async function addContactPages(pages: string[]): Promise<AddedPeople> {
  await contactPages.ask(pages);
  const answers = pages.flatMap((page) => contactPages.get(page) ?? []);
  const isPerson = (a: PageAnswer) => a.contact.kind === "person";
  const added = await addPeople(answers.filter(isPerson).map(newPerson));
  const groups = answers.filter((a) => !isPerson(a)).map((a) => nameOf(a.contact));
  const skipped = groups.map((what) => ({ what, why: "a group is not a person" }));
  return { ...added, skipped: [...added.skipped, ...skipped] };
}

export function newPerson(answer: PageAnswer): NewPerson {
  const handles = recordHandles(answer.contact, answer.fullNumbers);
  return {
    name: answer.contact.names[0] ?? "",
    handles: handles.filter((h) => h.sure).map((h) => h.handle),
  };
}
