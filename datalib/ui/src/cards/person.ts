// What a person's page shows (docs/dev/contacts.md § "In the UI"): who it
// is about, each source's record of them as a section of its own, and,
// while they are not in your contacts, the handles adding them would take.
// Pure; `PersonCard.ce.vue` draws it and the resolvers (contacts.ts,
// contactPages.ts) supply the answers.

import {
  handleKind,
  handleValue,
  nameOf,
  stoppedBy,
  type FullNumbers,
  type NormalizedContact,
  type Who,
} from "./contacts";
import { contactSubject, pageSubject } from "./cardSources";
import type { PageAnswer } from "./contactPages";

/** Who a person's page is about: whoever holds a handle, one of your
 *  contacts, or whoever one card in an address book describes, for a card
 *  with no handle. */
export type Subject =
  | { kind: "handle"; handle: string }
  | { kind: "contact"; id: string }
  | { kind: "page"; page: string };

const CONTACT = contactSubject("");
const PAGE = pageSubject("");

/** A subject as a card's source names it: a handle, `contact:<id>` or
 *  `page:<id>`. */
export function parseSubject(written: string): Subject {
  if (written.startsWith(CONTACT)) return { kind: "contact", id: written.slice(CONTACT.length) };
  if (written.startsWith(PAGE)) return { kind: "page", page: written.slice(PAGE.length) };
  return { kind: "handle", handle: written };
}

/** What the resolvers have answered so far; undefined until they have. */
export type Answers = {
  who: (handle: string) => Who | undefined;
  contact: (id: string) => NormalizedContact | undefined;
  page: (page: string) => PageAnswer | undefined;
};

/** One source's record of the person, for one of their handles. */
export type PersonSection = {
  sourceId: string;
  contact: NormalizedContact;
  /** The source the chip that opened the card was seen in. */
  seenHere: boolean;
  /** Why the record is on the card: the card's handles it holds. */
  via: string[];
  /** The numbers of the person's whose digits it writes with no country
   *  code, which is all that ties it to them. Empty for a record that
   *  holds a handle of theirs. */
  sameDigits: string[];
};

export type PersonModel = {
  /** The card's heading: your contact's name; else the name the leading
   *  source gives, else the handle itself. */
  title: string;
  /** Your contact, when the person is one. */
  mine: NormalizedContact | null;
  /** The person's handles: your contact's, or the one handle opened. */
  handles: string[];
  sections: PersonSection[];
  /** The full numbers each number with no country code may be. */
  fullNumbers: FullNumbers;
};

/** The handles of your contact, `first` leading when they are opened by it. */
export function personHandles(mine: NormalizedContact | null, first: string | null): string[] {
  const held = (mine?.handles ?? []).flatMap((h) => (h.handle ? [h.handle] : []));
  return first ? [first, ...held.filter((h) => h !== first)] : held;
}

/** A card's numbers with no country code whose full number the library
 *  knows, and knows only one of. */
function sureNumbers(contact: NormalizedContact, full: FullNumbers): string[] {
  return recordHandles(contact, full).flatMap((h) => (h.written && h.sure ? [h.handle] : []));
}

function contactOf(subject: Subject, answers: Answers): NormalizedContact | null {
  switch (subject.kind) {
    case "handle":
      return answers.who(subject.handle)?.mine ?? null;
    case "contact":
      return answers.contact(subject.id) ?? null;
    case "page": {
      // A card names no handle of its own; once its number is on a contact,
      // that contact is who it is about.
      const answer = answers.page(subject.page);
      if (!answer) return null;
      for (const h of sureNumbers(answer.contact, answer.fullNumbers)) {
        const mine = answers.who(h)?.mine;
        if (mine) return mine;
      }
      return null;
    }
  }
}

/** The page about `subject`, opened from a chip seen in `seenIn`. Each
 *  source's record appears once, under the first handle that carries it;
 *  the source the chip was seen in comes first, then the rest in the order
 *  `/people` ranked them, those found by their digits alone last. */
export function personModel(
  subject: Subject,
  seenIn: string | null,
  answers: Answers,
): PersonModel {
  const mine = contactOf(subject, answers);
  const handles = personHandles(mine, subject.kind === "handle" ? subject.handle : null);
  const sections: PersonSection[] = [];
  const fullNumbers: FullNumbers = {};
  const seen = new Set<string>();
  const add = (contact: NormalizedContact, sameDigits: string[]) => {
    const id = `${contact.source_id}\u0000${contact.key}`;
    if (seen.has(id)) {
      const had = sections.find((s) => s.sameDigits.length && recordId(s.contact) === id);
      if (had && sameDigits.length)
        had.sameDigits = [...new Set([...had.sameDigits, ...sameDigits])];
      return;
    }
    seen.add(id);
    const held = new Set(contact.handles.flatMap((h) => (h.handle ? [h.handle] : [])));
    sections.push({
      sourceId: contact.source_id,
      contact,
      seenHere: seenIn !== null && contact.source_id === seenIn,
      via: handles.filter((h) => held.has(h)),
      sameDigits,
    });
  };
  if (subject.kind === "page") {
    const answer = answers.page(subject.page);
    if (answer) {
      add(answer.contact, []);
      Object.assign(fullNumbers, answer.fullNumbers);
    }
  }
  for (const h of handles) {
    const who = answers.who(h);
    for (const contact of who?.sourceContacts ?? []) add(contact, []);
    Object.assign(fullNumbers, who?.fullNumbers);
  }
  for (const h of handles) {
    for (const contact of answers.who(h)?.sameDigits ?? []) add(contact, [h]);
  }
  // A stable sort keeps the ranking within each part.
  sections.sort(
    (a, b) =>
      Number(a.sameDigits.length > 0) - Number(b.sameDigits.length > 0) ||
      Number(b.seenHere) - Number(a.seenHere),
  );
  const lead = sections[0]?.contact;
  const bare = subject.kind === "handle" ? handleValue(subject.handle) : "";
  return {
    title: mine ? nameOf(mine) : lead ? nameOf(lead) : bare,
    mine,
    handles,
    sections,
    fullNumbers,
  };
}

function recordId(c: NormalizedContact): string {
  return `${c.source_id}\u0000${c.key}`;
}

/** One handle a record says is the person's. */
export type RecordHandle = {
  handle: string;
  /** As the record writes it. */
  shown: string;
  label: string | null;
  /** The number as the record writes it, with no country code, when that
   *  is all it says: the handle is a full number with the same digits. */
  written: string | null;
  /** Nothing else it could be. A number two countries have is neither. */
  sure: boolean;
};

/** Every handle a record says is the person's, once each: its own, and the
 *  full numbers its others may be. A stand-in for the library's owner, or
 *  a model's answers, is no handle of anyone's. */
export function recordHandles(c: NormalizedContact, full: FullNumbers): RecordHandle[] {
  const out = new Map<string, RecordHandle>();
  for (const h of c.handles) {
    if (!h.handle || !canAdd(h.handle) || out.has(h.handle)) continue;
    const shown = h.medium === "other" ? handleValue(h.handle) : h.value;
    out.set(h.handle, { handle: h.handle, shown, label: h.label, written: null, sure: true });
  }
  for (const h of c.handles) {
    if (h.handle || h.medium !== "phone") continue;
    const numbers = full[h.value] ?? [];
    for (const handle of numbers) {
      if (out.has(handle)) continue;
      out.set(handle, {
        handle,
        shown: handleValue(handle),
        label: h.label,
        written: h.value,
        sure: numbers.length === 1,
      });
    }
  }
  return [...out.values()];
}

function canAdd(handle: string): boolean {
  const kind = handleKind(handle);
  return kind !== "me" && kind !== "ai_model";
}

/** A handle that adding the person to your contacts would take. */
export type Offer = {
  handle: string;
  shown: string;
  label: string | null;
  /** The sources that say it is theirs, the page's order. */
  sources: string[];
  written: string | null;
  /** Your contact who has it already, so it is not offered. */
  owner: NormalizedContact | null;
  ticked: boolean;
};

/** The handles to add, while the person is not in your contacts: the one
 *  opened, then every other on any record that holds it. One that another
 *  contact has, or that is only one of several it could be, is not ticked. */
export function offers(subject: Subject, model: PersonModel, answers: Answers): Offer[] {
  if (model.mine) return [];
  const out = new Map<string, Offer>();
  const put = (h: RecordHandle, source: string | null, sure: boolean) => {
    const owner = answers.who(h.handle)?.mine ?? null;
    const had = out.get(h.handle);
    if (!had) {
      out.set(h.handle, {
        handle: h.handle,
        shown: h.shown,
        label: h.label,
        sources: source ? [source] : [],
        written: h.written,
        owner,
        ticked: sure && !owner,
      });
      return;
    }
    if (source && !had.sources.includes(source)) had.sources.push(source);
    had.label ??= h.label;
    had.written ??= h.written;
    had.ticked ||= sure && !owner;
  };
  if (subject.kind === "handle") {
    const shown = handleValue(subject.handle);
    put({ handle: subject.handle, shown, label: null, written: null, sure: true }, null, true);
  }
  for (const s of model.sections) {
    if (s.contact.kind !== "person") continue;
    const handles = recordHandles(s.contact, model.fullNumbers);
    // A card found by its digits alone is theirs only if its number can
    // be nobody else's.
    const theirs =
      s.sameDigits.length === 0 ||
      handles.some((h) => h.sure && h.written && s.sameDigits.includes(h.handle));
    for (const h of handles) put(h, s.sourceId, theirs && h.sure);
  }
  return [...out.values()];
}

/** What an offered handle's line says beside it: who has it, or which
 *  sources say it is theirs, and how a card wrote it. */
export function offerNote(o: Offer, sourceName: (id: string) => string): string {
  if (o.owner) return `on ${nameOf(o.owner)}`;
  const names = o.sources.map(sourceName);
  const shown = names.length > 3 ? [...names.slice(0, 2), `${names.length - 2} more`] : names;
  return [...(o.written ? [`written ${o.written}`] : []), shown.join(", ")]
    .filter(Boolean)
    .join(" · ");
}

/** The lines a section shows under its source's name, in reading order:
 *  the names it adds beyond the first, its role, and the rest of what it
 *  says. Empty parts are left out. `stamp` writes a stamp for reading. */
export function sectionLines(
  c: NormalizedContact,
  stamp: (iso: string) => string = (iso) => iso,
): { label: string; value: string }[] {
  const out: { label: string; value: string }[] = [];
  if (c.names.length > 1) out.push({ label: "Also called", value: c.names.slice(1).join(", ") });
  const role = [c.title, c.org].filter((s): s is string => !!s?.trim()).join(", ");
  if (role) out.push({ label: "Role", value: role });
  for (const d of c.details ?? []) out.push({ label: d.label, value: d.value });
  if (c.note?.trim()) out.push({ label: "Note", value: c.note.trim() });
  if (c.seen) {
    const items = `${c.seen.items} ${c.seen.items === 1 ? "item" : "items"}`;
    const last = c.seen.last_at ? `, last ${stamp(c.seen.last_at)}` : "";
    out.push({ label: "Seen", value: `${items}${last}` });
  }
  return out;
}

/** Where one handle stands against the card's contact: on it, on someone
 *  else's, or on nobody's. */
export type LinkState = "this" | "other" | "none";

export function linkState(
  handle: string,
  mine: NormalizedContact | null,
  whoOf: (handle: string) => Who | undefined,
): LinkState {
  const owner = whoOf(handle)?.mine;
  if (!owner) return "none";
  return mine && owner.key === mine.key ? "this" : "other";
}

const HANDLE_ACTIONS = ["handle-add", "handle-remove", "handle-stopped", "handle-works"] as const;
export type HandleAction = (typeof HANDLE_ACTIONS)[number];

export function isHandleAction(id: string): id is HandleAction {
  return (HANDLE_ACTIONS as readonly string[]).includes(id);
}

/** What a handle's chip on the card offers beyond a chip anywhere: to go
 *  on the card's contact, or to come off it. Nothing for a handle someone
 *  else has, or with no contact to act on. */
export function handleMenu(
  handle: string,
  model: PersonModel,
  whoOf: (handle: string) => Who | undefined,
): { id: HandleAction; label: string; separator?: boolean }[] {
  if (!model.mine) return [];
  switch (linkState(handle, model.mine, whoOf)) {
    case "none":
      return [{ id: "handle-add", label: `Add to ${model.title}`, separator: true }];
    case "this":
      return [
        { id: "handle-remove", label: "Not theirs", separator: true },
        stoppedBy(model.mine, handle)
          ? { id: "handle-works", label: "Works again" }
          : { id: "handle-stopped", label: "No longer works" },
      ];
    case "other":
      return [];
  }
}

/** The handles a source's record holds that no contact has: what its
 *  "Add its other handles" button puts on the card's contact. Left alone:
 *  one someone else holds, since adding it would take it from them, and
 *  one you said is not theirs. */
export function linkableHandles(
  section: PersonSection,
  model: PersonModel,
  whoOf: (handle: string) => Who | undefined,
  notTheirs: ReadonlySet<string>,
): string[] {
  const { mine } = model;
  if (!mine || section.contact.kind !== "person") return [];
  return recordHandles(section.contact, model.fullNumbers)
    .filter((h) => h.sure && !notTheirs.has(h.handle))
    .map((h) => h.handle)
    .filter((h) => linkState(h, mine, whoOf) === "none");
}

/** Every handle the page shows or offers, for asking who each belongs to. */
export function shownHandles(model: PersonModel): string[] {
  const out = new Set<string>();
  for (const s of model.sections) {
    for (const h of s.contact.handles) if (h.handle) out.add(h.handle);
    for (const h of recordHandles(s.contact, model.fullNumbers)) out.add(h.handle);
  }
  return [...out];
}
