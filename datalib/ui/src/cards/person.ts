// What the person card shows (docs/dev/plans/contact_editing.md § "The
// card shows a person"): who the card is about, and each source's
// record of them as a section of its own. Pure; `PersonCard.ce.vue`
// draws it and `people` (contacts.ts) supplies the answers.

import { handleValue, nameOf, stoppedBy, type NormalizedContact, type Who } from "./contacts";

/** One source's record of the person, for one of their handles. */
export type PersonSection = {
  sourceId: string;
  contact: NormalizedContact;
  /** The source the chip that opened the card was seen in. */
  seenHere: boolean;
  /** Why the record is on the card: the card's handles it holds. */
  via: string[];
};

export type PersonModel = {
  /** The card's heading: your contact's name; else the name the leading
   *  source gives, else the handle itself. */
  title: string;
  /** Your contact, when the handle is linked to one. */
  mine: NormalizedContact | null;
  /** The person's handles: your contact's, or the one handle opened. */
  handles: string[];
  sections: PersonSection[];
};

/** The handles a card about `handle` covers: every handle of the contact
 *  it is linked to, the opened one first, or just the opened one. */
export function personHandles(handle: string, who: Who | undefined): string[] {
  const linked = (who?.mine?.handles ?? []).flatMap((h) => (h.handle ? [h.handle] : []));
  return [handle, ...linked.filter((h) => h !== handle)];
}

/** The card for `handle`, opened from a chip seen in `seenIn`. `whoOf`
 *  answers for any of the person's handles, undefined until asked. Each
 *  source's record appears once, under the first handle that carries it;
 *  the source the chip was seen in comes first, then the rest in the
 *  order `/people` ranked them. */
export function personModel(
  handle: string,
  seenIn: string | null,
  whoOf: (handle: string) => Who | undefined,
): PersonModel {
  const opened = whoOf(handle);
  const mine = opened?.mine ?? null;
  const handles = personHandles(handle, opened);
  const seen = new Set<string>();
  const sections: PersonSection[] = [];
  for (const h of handles) {
    for (const contact of whoOf(h)?.sourceContacts ?? []) {
      const id = `${contact.source_id}\u0000${contact.key}`;
      if (seen.has(id)) continue;
      seen.add(id);
      const held = new Set(contact.handles.flatMap((ch) => (ch.handle ? [ch.handle] : [])));
      sections.push({
        sourceId: contact.source_id,
        contact,
        seenHere: seenIn !== null && contact.source_id === seenIn,
        via: handles.filter((x) => held.has(x)),
      });
    }
  }
  // A stable sort keeps the ranking within each half.
  sections.sort((a, b) => Number(b.seenHere) - Number(a.seenHere));
  const lead = sections[0]?.contact;
  return {
    title: mine ? nameOf(mine) : lead ? nameOf(lead) : handleValue(handle),
    mine,
    handles,
    sections,
  };
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
 *  "Add its other handles" button puts on the card's contact. A handle someone else
 *  holds is left alone, since linking it would take it from them. */
export function linkableHandles(
  section: PersonSection,
  mine: NormalizedContact | null,
  whoOf: (handle: string) => Who | undefined,
): string[] {
  if (!mine) return [];
  const out: string[] = [];
  for (const h of section.contact.handles) {
    if (h.handle && !out.includes(h.handle) && linkState(h.handle, mine, whoOf) === "none") {
      out.push(h.handle);
    }
  }
  return out;
}

/** Every handle any section holds, for asking who each belongs to. */
export function sectionHandles(model: PersonModel): string[] {
  const out = new Set<string>();
  for (const s of model.sections) {
    for (const h of s.contact.handles) if (h.handle) out.add(h.handle);
  }
  return [...out];
}
