// What a chip does, wherever it is drawn: which chip a press landed on,
// the card its double-click opens, its right-click menu, and what each
// entry of that menu does. A surface draws the menu its own way — a
// floating menu over a document, a grid's context menu, the search
// field's tooltip — and hands the pick back here (docs/dev/chips.md
// § Clicks).
import { ref, shallowRef } from "vue";
import * as api from "@/api";
import type { ManageRow } from "@/api";
import { copyRichToClipboard, copyToClipboard } from "@/clipboard";
import { rowMenu, type MenuAction } from "@/config/rowMenu";
import { setT0 } from "@/config/timeView";
import { isDesktopApp, revealActionLabel } from "@/desktop";
import { openExternal } from "@/externalLinks";
import type { MenuEntry } from "@/grid/menu";
import { filterToken } from "@/grid/query";
import { pushToast } from "@/toasts";
import { menuTarget, rowActions, withBrowse } from "./rowActions";
import { contactSubject, pageSubject, personSource, searchSource } from "./cardSources";
import {
  chipLook,
  chipMenu,
  composeUri,
  contactLook,
  contactsById,
  copyText,
  handleValue,
  hasContactsApp,
  NOBODY,
  people,
  searchQueryFor,
  type NormalizedContact,
} from "./contacts";
import { entityFromUri } from "./chipLinks";
import { contactPageLook, contactPages } from "./contactPages";
import {
  browseQuery,
  entities,
  entityCardSource,
  entityCopyText,
  entityLogSource,
  entityMenu,
} from "./entities";
import {
  documentCardSource,
  documentCopyHtml,
  documentCopyText,
  documentId,
  documentMenu,
} from "./documentChip";
import { nodeCardSource, nodeMenu } from "./nodeChip";
import { parseNode } from "@/uri";
import { CONTACT, type ChipRef } from "@/search/queryText";
import { dateCopyText, dateMenu, dateWallText, dayQuery, parseStamp, sideQuery } from "./dates";
import { labelMenu, labelQuery } from "./labelChip";

/** What a chip names: what the search field can name, whoever a contact's
 *  page is about, a date or a label, which only a document draws, a
 *  document, which a grid draws, or a node of a source's tree, with the
 *  name and the document its card opens on. */
export type AnyChipRef =
  | ChipRef
  | { kind: "page"; page: string }
  | { kind: "date"; datetime: string }
  | { kind: "label"; name: string }
  | { kind: "document"; uri: string }
  | { kind: "node"; uri: string; doc: string | null };
export type { ChipRef };

/** A chip as it was drawn: what it names, and the name its producer
 *  showed before the resolver answered. */
export type Chip = { ref: AnyChipRef; shownAs: string };

/** Every menu entry any chip offers, in the one order every menu shows
 *  them: open what it names first (the double-click's card) and a step's
 *  log, then write to it, copy it, search for it, and add it. */
export const CHIP_ACTIONS = [
  "open",
  "log",
  "compose",
  "copy-id",
  "copy-name",
  "copy-both",
  "search",
  "before",
  "after",
  "browse",
  "add",
  "use-t0",
  "clear-t0",
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

/** An entry of a chip's menu as `ChipMenu` draws it: a chip's own, or one
 *  the surface adds, as the search field and the log-line card do. */
export type MenuItem = Omit<ChipEntry, "id"> & { id: string };

type Size = { width: number; height: number };

/** Where a menu of `size` opens for a press at `at`: there, or moved
 *  left and up just enough to stay on screen. */
export function menuPlacement(
  at: { x: number; y: number },
  size: Size,
  viewport: Size,
): { left: number; top: number } {
  const MARGIN = 8;
  return {
    left: Math.max(0, Math.min(at.x, viewport.width - size.width - MARGIN)),
    top: Math.max(0, Math.min(at.y, viewport.height - size.height - MARGIN)),
  };
}

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

export const CHIP_SELECTOR =
  "a.chip[data-handle], a.chip[data-entity], a.chip[data-contact], a.chip[data-contact-page], a.chip[data-doc], a.chip[data-node], time.chip[datetime], span.chip[data-label]";

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
  if (el.dataset.contactPage) {
    return { ref: { kind: "page", page: el.dataset.contactPage }, shownAs };
  }
  if (el.dataset.contact) return { ref: { kind: "contact", id: el.dataset.contact }, shownAs };
  if (el.dataset.doc) return { ref: { kind: "document", uri: el.dataset.doc }, shownAs };
  if (el.dataset.node) {
    return {
      ref: { kind: "node", uri: el.dataset.node, doc: el.dataset.nodeDoc ?? null },
      shownAs,
    };
  }
  const datetime = el.tagName === "TIME" ? el.getAttribute("datetime") : null;
  if (datetime) return { ref: { kind: "date", datetime }, shownAs };
  if (el.dataset.label) return { ref: { kind: "label", name: el.dataset.label }, shownAs };
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
      return chipLook(ref.handle, shownAs, people.get(ref.handle) ?? NOBODY).text;
    case "contact":
      return contactLook(shownAs || "a contact", contactsById.get(ref.id)).text;
    case "page":
      return contactPageLook(shownAs, contactPages.get(ref.page)).text;
    case "document":
    case "node":
      return shownAs || ref.uri;
    case "label":
      return ref.name;
    case "date": {
      const s = parseStamp(ref.datetime);
      return s ? dateWallText(s) : ref.datetime;
    }
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

/** A chip's right-click menu. */
export function chipMenuOf(chip: Chip): ChipEntry[] {
  const { ref, shownAs } = chip;
  switch (ref.kind) {
    case "entity":
      return entityChipMenu(ref.uri, chipName(chip), entities.get(ref.uri)?.row);
    case "person":
      return chipMenu(ref.handle, shownAs, people.get(ref.handle) ?? NOBODY, hasContactsApp());
    case "contact": {
      const name = chipName(chip);
      return [
        { id: "open", label: "Open contact" },
        { id: "copy-name", label: `Copy “${name}”`, separator: true },
        { id: "search", label: `Everything with ${name}`, separator: true },
      ];
    }
    case "page": {
      const add: ChipEntry[] = hasContactsApp()
        ? [{ id: "add", label: "Add to contacts…", separator: true }]
        : [];
      return [
        { id: "open", label: "Open" },
        { id: "copy-name", label: `Copy “${chipName(chip)}”`, separator: true },
        ...add,
      ];
    }
    case "document":
      return documentMenu(ref.uri, chipName(chip));
    case "label":
      return labelMenu(ref.name);
    case "node":
      return nodeMenu(ref.uri, chipName(chip));
    case "date": {
      const s = parseStamp(ref.datetime);
      return s ? dateMenu(ref.datetime, s) : [];
    }
  }
}

/** The search a chip stands for when it is dragged into a search bar:
 *  everything with a person or a contact, a group's documents, a date's
 *  day, a label's messages. Null for a step or a document, which no search key names. */
export function chipDragQuery(ref: AnyChipRef): string | null {
  switch (ref.kind) {
    case "entity":
      return browseQuery(ref.uri);
    case "person":
      return everythingWith(ref.handle, people.get(ref.handle)?.mine ?? null);
    case "contact":
      return contactQuery(ref.id);
    case "page":
    case "document":
    case "node":
      return null;
    case "label":
      return labelQuery(ref.name);
    case "date": {
      const s = parseStamp(ref.datetime);
      return s ? dayQuery(s) : null;
    }
  }
}

/** What double-clicking a chip opens: a person's card, led by the
 *  source the chip was seen in; a group's sync dashboard; a step's log;
 *  a document's card; a date's or a label's search. */
export function chipCardSource(
  ref: AnyChipRef,
  seenIn: string | null,
  shownAs = "",
): string | null {
  switch (ref.kind) {
    case "entity":
      return entityCardSource(ref.uri);
    case "person":
    case "contact":
    case "page":
      return personSource(subjectOf(ref), { seenIn });
    case "document":
      return documentCardSource(ref.uri);
    case "node":
      return nodeCardSource(ref.uri, shownAs, ref.doc);
    case "label":
    case "date": {
      const q = chipDragQuery(ref);
      return q ? searchSource(q) : null;
    }
  }
}

type PersonRef = Extract<AnyChipRef, { kind: "person" | "contact" | "page" }>;

/** What a person's page opened from this chip is about. */
function subjectOf(ref: PersonRef): string {
  switch (ref.kind) {
    case "person":
      return ref.handle;
    case "contact":
      return contactSubject(ref.id);
    case "page":
      return pageSubject(ref.page);
  }
}

/** The person a chip just opened, as their page's source names them. Their
 *  page takes the keyboard from it, so Enter adds them; a page opened any
 *  other way leaves it alone. */
export const justOpened = shallowRef<{ subject: string } | null>(null);

/** Open what a chip names; false for one that names nothing to open. */
export function openChip(
  ref: AnyChipRef,
  seenIn: string | null,
  openCard: (source: string) => void,
  shownAs = "",
): boolean {
  const source = chipCardSource(ref, seenIn, shownAs);
  if (!source) return false;
  const person = ref.kind === "person" || ref.kind === "contact" || ref.kind === "page";
  justOpened.value = person ? { subject: subjectOf(ref) } : null;
  openCard(source);
  return true;
}

/** What a surface lets a chip's entries do there. */
export type ChipHost = {
  openCard: (source: string) => void;
  /** Where Everything with and Browse land: a search card beside this
   *  one unless the surface has a search of its own to narrow
   *  ("narrow", Everything with) or replace ("replace", Browse). */
  search?: (q: string, how: "narrow" | "replace") => void;
  /** The source the chip was seen in, which a person's card leads with. */
  seenIn?: string | null;
};

async function copy(text: string) {
  if (await copyToClipboard(text)) pushToast(`Copied ${text}`, "info");
  else pushToast("The clipboard refused the copy");
}

async function copyLink(text: string, html: string) {
  if (await copyRichToClipboard(text, html)) pushToast(`Copied ${text}`, "info");
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
    case "open":
    case "add":
      openChip(ref, host.seenIn ?? null, host.openCard, name);
      return;
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
      if (ref.kind === "document") await copy(documentId(ref.uri) ?? ref.uri);
      if (ref.kind === "node") await copy(parseNode(ref.uri) ?? ref.uri);
      if (ref.kind === "date") await copy(ref.datetime.trim());
      return;
    case "copy-name": {
      const s = ref.kind === "date" ? parseStamp(ref.datetime) : null;
      await copy(s ? dateCopyText(s) : name);
      return;
    }
    case "copy-both":
      if (ref.kind === "person") await copy(copyText(ref.handle, name));
      if (ref.kind === "entity") await copy(entityCopyText(ref.uri, name));
      if (ref.kind === "document" || ref.kind === "node") {
        await copyLink(documentCopyText(ref.uri, name), documentCopyHtml(ref.uri, name));
      }
      return;
    case "search":
      if (ref.kind === "person") {
        search(everythingWith(ref.handle, people.get(ref.handle)?.mine ?? null), "narrow");
      }
      if (ref.kind === "contact") search(contactQuery(ref.id), "narrow");
      if (ref.kind === "date" || ref.kind === "label") {
        const q = chipDragQuery(ref);
        if (q) search(q, "narrow");
      }
      return;
    case "before":
    case "after": {
      const s = ref.kind === "date" ? parseStamp(ref.datetime) : null;
      if (ref.kind === "date" && s) search(sideQuery(ref.datetime, s, id), "narrow");
      return;
    }
    case "browse": {
      const q = ref.kind === "entity" ? browseQuery(ref.uri) : null;
      if (q) search(q, "replace");
      return;
    }
    case "use-t0": {
      const s = ref.kind === "date" ? parseStamp(ref.datetime) : null;
      if (s?.kind === "instant") setT0({ ms: s.ms, offsetMin: s.offsetMin });
      return;
    }
    case "clear-t0":
      setT0(null);
      return;
  }
}
