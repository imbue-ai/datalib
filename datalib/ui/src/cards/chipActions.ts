// What a chip does, wherever it is drawn: which chip a press landed on,
// the card its double-click opens, its right-click menu, and what each
// entry of that menu does. A surface draws the menu its own way — a
// floating menu over a document, a grid's context menu, the search
// field's tooltip — and hands the pick back here (docs/dev/chips.md
// § Clicks).
import { copyToClipboard } from "@/clipboard";
import { openExternal } from "@/externalLinks";
import { filterToken } from "@/grid/query";
import { pushToast } from "@/toasts";
import { personSource, searchSource } from "./cardSources";
import {
  chipLook,
  chipMenu,
  composeUri,
  contactLook,
  contactsById,
  copyText,
  handleValue,
  NOBODY,
  people,
  searchQueryFor,
} from "./contacts";
import { entityFromUri } from "./chipLinks";
import { browseQuery, entities, entityCardSource, entityCopyText, entityMenu } from "./entities";
import { CONTACT, type ChipRef } from "@/search/queryText";

/** A chip as it was drawn: what it names, and the name its producer
 *  showed before the resolver answered. */
export type { ChipRef };

export type Chip = { ref: ChipRef; shownAs: string };

/** Every menu entry any chip offers, in the one order every menu shows
 *  them: open what it names first (the double-click's card), then write
 *  to it, copy it, search for it, and link it. */
export const CHIP_ACTIONS = [
  "open",
  "compose",
  "copy-id",
  "copy-name",
  "copy-both",
  "search",
  "browse",
  "edit",
] as const;
export type ChipAction = (typeof CHIP_ACTIONS)[number];
export type ChipEntry = { id: ChipAction; label: string; separator?: boolean };

export const CHIP_SELECTOR = "a.chip[data-handle], a.chip[data-entity], a.chip[data-contact]";

/** The chip element `target` is in, of any kind; null off a chip. */
export function chipElAt(target: EventTarget | null | undefined): HTMLElement | null {
  const el = target as Element | null | undefined;
  return el?.closest?.<HTMLElement>(CHIP_SELECTOR) ?? null;
}

/** What a chip element names. */
export function chipOf(el: HTMLElement): Chip | null {
  const shownAs = el.dataset.shownAs ?? "";
  if (el.dataset.entity) return { ref: { kind: "entity", uri: el.dataset.entity }, shownAs };
  if (el.dataset.handle) return { ref: { kind: "person", handle: el.dataset.handle }, shownAs };
  if (el.dataset.contact) return { ref: { kind: "contact", id: el.dataset.contact }, shownAs };
  return null;
}

/** The name a chip goes by now: the resolver's answer, else what its
 *  producer showed. */
export function chipName(chip: Chip): string {
  const { ref, shownAs } = chip;
  switch (ref.kind) {
    case "entity":
      return entities.get(ref.uri)?.label || shownAs || entityFromUri(ref.uri)?.id || ref.uri;
    case "person":
      return chipLook(ref.handle, shownAs, people.get(ref.handle) ?? NOBODY, false).text;
    case "contact":
      return contactLook(shownAs || "a contact", contactsById.get(ref.id)).text;
  }
}

/** The search for everything with one of your contacts. */
function contactQuery(id: string): string {
  return filterToken("with", `${CONTACT}${id}`, false);
}

/** A chip's right-click menu. `canLink` is whether this surface hosts
 *  the link popover, so a person's menu can offer to link them. */
export function chipMenuOf(chip: Chip, canLink: boolean): ChipEntry[] {
  const { ref, shownAs } = chip;
  switch (ref.kind) {
    case "entity":
      return entityMenu(ref.uri, chipName(chip));
    case "person":
      return chipMenu(ref.handle, shownAs, people.get(ref.handle) ?? NOBODY, canLink);
    case "contact": {
      const name = chipName(chip);
      return [
        { id: "copy-name", label: `Copy “${name}”` },
        { id: "search", label: `Everything with ${name}`, separator: true },
      ];
    }
  }
}

/** What double-clicking a chip opens: a person's card, led by the
 *  source the chip was seen in; a group's sync dashboard; a step's log.
 *  Null for a contact, which has no card of its own yet. */
export function chipCardSource(ref: ChipRef, seenIn: string | null): string | null {
  switch (ref.kind) {
    case "entity":
      return entityCardSource(ref.uri);
    case "person":
      return personSource(ref.handle, { seenIn });
    case "contact":
      return null;
  }
}

/** What a surface lets a chip's entries do there. */
export type ChipHost = {
  openCard: (source: string) => void;
  /** Where Everything with and Browse land: a search card beside this
   *  one unless the surface has a search of its own to narrow
   *  ("narrow", Everything with) or replace ("replace", Browse). */
  search?: (q: string, how: "narrow" | "replace") => void;
  /** Linking a handle to a contact: the popover, where this surface
   *  hosts one. */
  editLink?: () => void;
  /** The source the chip was seen in, which a person's card leads with. */
  seenIn?: string | null;
};

async function copy(text: string) {
  if (await copyToClipboard(text)) pushToast(`Copied ${text}`, "info");
  else pushToast("The clipboard refused the copy");
}

/** Do what a chip's menu entry says. */
export async function runChipAction(id: ChipAction, chip: Chip, host: ChipHost): Promise<void> {
  const { ref } = chip;
  const name = chipName(chip);
  const search = (q: string, how: "narrow" | "replace") =>
    host.search ? host.search(q, how) : host.openCard(searchSource(q));
  switch (id) {
    case "open": {
      const source = chipCardSource(ref, host.seenIn ?? null);
      if (source) host.openCard(source);
      return;
    }
    case "compose": {
      const mailto = ref.kind === "person" ? composeUri(ref.handle) : null;
      if (mailto) void openExternal(mailto);
      return;
    }
    case "copy-id":
      if (ref.kind === "person") await copy(handleValue(ref.handle));
      if (ref.kind === "entity") await copy(entityFromUri(ref.uri)?.id ?? ref.uri);
      return;
    case "copy-name":
      await copy(name);
      return;
    case "copy-both":
      if (ref.kind === "person") await copy(copyText(ref.handle, name));
      if (ref.kind === "entity") await copy(entityCopyText(ref.uri, name));
      return;
    case "search":
      if (ref.kind === "person") search(searchQueryFor(ref.handle), "narrow");
      if (ref.kind === "contact") search(contactQuery(ref.id), "narrow");
      return;
    case "browse": {
      const q = ref.kind === "entity" ? browseQuery(ref.uri) : null;
      if (q) search(q, "replace");
      return;
    }
    case "edit":
      host.editLink?.();
      return;
  }
}
