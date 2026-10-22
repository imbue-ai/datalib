// What a chip does, wherever it is drawn: which chip a press landed on,
// the card its double-click opens, its right-click menu, and what each
// entry of that menu does. A surface draws the menu its own way — a
// floating menu over a document, a grid's context menu, the search
// field's tooltip — and hands the pick back here (docs/dev/chips.md
// § Clicks).
import { ref } from "vue";
import * as api from "@/api";
import type { ManageRow } from "@/api";
import { copyToClipboard } from "@/clipboard";
import { rowMenu, type MenuAction } from "@/config/rowMenu";
import { isDesktopApp, revealActionLabel } from "@/desktop";
import { openExternal } from "@/externalLinks";
import type { MenuEntry } from "@/grid/menu";
import { filterToken } from "@/grid/query";
import { pushToast } from "@/toasts";
import { menuTarget, rowActions, withBrowse } from "./rowActions";
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
  type NormalizedContact,
} from "./contacts";
import { entityFromUri } from "./chipLinks";
import {
  browseQuery,
  entities,
  entityCardSource,
  entityCopyText,
  entityLogSource,
  entityMenu,
} from "./entities";
import { CONTACT, type ChipRef } from "@/search/queryText";

/** A chip as it was drawn: what it names, and the name its producer
 *  showed before the resolver answered. */
export type { ChipRef };

export type Chip = { ref: ChipRef; shownAs: string };

/** Every menu entry any chip offers, in the one order every menu shows
 *  them: open what it names first (the double-click's card) and a step's
 *  log, then write to it, copy it, search for it, and link it. */
export const CHIP_ACTIONS = [
  "open",
  "log",
  "compose",
  "copy-id",
  "copy-name",
  "copy-both",
  "search",
  "browse",
  "edit",
] as const;
export type ChipAction = (typeof CHIP_ACTIONS)[number];
/** An entry of a group's or step's Sources row, offered on its chip. */
export type RowEntryId = `row:${MenuAction}`;
export type ChipEntryId = ChipAction | RowEntryId;
export type ChipEntry = {
  id: ChipEntryId;
  label: string;
  separator?: boolean;
  /** Why the entry cannot be taken now, shown on hover; null when it can. */
  disabled?: string | null;
  /** It stops or empties something. */
  danger?: boolean;
};

/** The row's entries a chip leaves out: the ones that need the config's
 *  text, which only the Sources card holds, and the ones the chip's own
 *  entries already are (its dashboard is the chip's Open; its id is the
 *  chip's Copy). */
const NOT_ON_A_CHIP: MenuAction[] = ["edit", "rename", "remove", "compare", "dashboard", "copy_id"];
const EDIT_IN_SOURCES = "Edit settings from the Sources card";

/** A group's or step's Sources row as chip entries: what `rowMenu` offers
 *  it, less `NOT_ON_A_CHIP`, a line where the row's menu draws one. */
export function rowEntries(row: ManageRow): ChipEntry[] {
  const canReveal = isDesktopApp();
  const target = menuTarget(withBrowse(row, new Map(), canReveal), EDIT_IN_SOURCES);
  const out: ChipEntry[] = [];
  let line = true;
  for (const e of rowMenu([target], { canReveal, revealLabel: revealActionLabel() })) {
    if (e.separator) {
      line = true;
      continue;
    }
    if (NOT_ON_A_CHIP.includes(e.action)) continue;
    out.push({
      id: `row:${e.action}`,
      label: e.name,
      disabled: e.disabled,
      danger: e.action === "reset" || e.action === "stop",
      ...(line ? { separator: true } : {}),
    });
    line = false;
  }
  return out;
}

/** A group or step chip's menu: Open, then its Sources row's entries,
 *  then the copies. The row's Browse and step log stand for the chip's
 *  own, which open the same with less (a bare search, no run). Before
 *  the row is known, the chip's own entries alone. */
export function entityChipMenu(uri: string, name: string, row: ManageRow | undefined): ChipEntry[] {
  const own = entityMenu(uri, name);
  if (!row) return own;
  const open = own.filter((e) => e.id === "open");
  const copies = own
    .filter((e) => e.id.startsWith("copy-"))
    .map((e, i) => ({ ...e, separator: i === 0 }));
  return [...open, ...rowEntries(row), ...copies];
}

/** Chip entries as a grid's menu entries (`grid/menu.ts`), each running
 *  through `run`. */
export function asMenuEntries(entries: ChipEntry[], run: (id: ChipEntryId) => void): MenuEntry[] {
  return entries.flatMap((e, i): MenuEntry[] => [
    ...(e.separator && i > 0 ? [{ name: "", separator: true }] : []),
    { name: e.label, disabled: e.disabled ?? null, danger: e.danger, action: () => run(e.id) },
  ]);
}

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

/** "Everything with" a person: the contact their handle is linked to,
 *  which reaches every handle they used, else the handle alone. */
export function everythingWith(handle: string, mine: NormalizedContact | null): string {
  return mine ? contactQuery(mine.key) : searchQueryFor(handle);
}

/** A chip's right-click menu. `canLink` is whether this surface hosts
 *  the link popover, so a person's menu can offer to link them. */
export function chipMenuOf(chip: Chip, canLink: boolean): ChipEntry[] {
  const { ref, shownAs } = chip;
  switch (ref.kind) {
    case "entity":
      return entityChipMenu(ref.uri, chipName(chip), entities.get(ref.uri)?.row);
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

/** Do one of a group's or step's Sources row entries from its chip, as
 *  the Sources table would: on the rows read fresh, so a group's steps
 *  and its status child are there and nothing acts on a stale state.
 *  What it says goes to a toast, since a chip has no banner. */
async function runRowAction(action: MenuAction, uri: string, host: ChipHost) {
  const entity = entityFromUri(uri);
  if (!entity) return;
  try {
    const manage = await api.fetchManageRows();
    const canReveal = isDesktopApp();
    const groups = new Map(manage.rows.filter((r) => r.kind === "group").map((g) => [g.id, g]));
    const rows = manage.rows.map((r) => withBrowse(r, groups, canReveal));
    const row = rows.find((r) => r.kind === entity.kind && r.id === entity.id);
    if (!row) {
      pushToast(`${entity.id} is no longer in the config.`);
      return;
    }
    const actions = rowActions({
      api,
      host: {
        openCards: (...sources) => {
          for (const s of sources) host.openCard(s);
          return [];
        },
      },
      rows: () => rows,
      run: () => manage.run,
      say: (ok, text) => pushToast(text, ok ? "info" : "error"),
      clear: () => {},
      busy: ref(false),
      reload: async () => entities.revalidate(),
    });
    await actions.runMenuAction(action, [row]);
  } catch (e) {
    pushToast((e as Error).message);
  }
}

/** Do what a chip's menu entry says. */
export async function runChipAction(id: ChipEntryId, chip: Chip, host: ChipHost): Promise<void> {
  const { ref } = chip;
  if (id.startsWith("row:")) {
    if (ref.kind === "entity") await runRowAction(id.slice(4) as MenuAction, ref.uri, host);
    return;
  }
  const name = chipName(chip);
  const search = (q: string, how: "narrow" | "replace") =>
    host.search ? host.search(q, how) : host.openCard(searchSource(q));
  switch (id) {
    case "open": {
      const source = chipCardSource(ref, host.seenIn ?? null);
      if (source) host.openCard(source);
      return;
    }
    case "log": {
      const source = ref.kind === "entity" ? entityLogSource(ref.uri) : null;
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
      if (ref.kind === "person") {
        search(everythingWith(ref.handle, people.get(ref.handle)?.mine ?? null), "narrow");
      }
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
