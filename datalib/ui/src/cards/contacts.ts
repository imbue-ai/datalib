// Who a handle in a document is. A renderer writes a person as a chip
// link, `[Name](mailto:…)`, which `chipLinks.js` marks as it renders
// (`a.chip[data-handle]`); this asks the index (`unified_index`'s
// `/people`: each source's record of the person) and the contacts app
// (`datalib_contacts`: the contact a person made, ranked first) and
// draws the link as a chip. The contacts app is an app of its own
// (docs/dev/contacts.md): without it chips still say what the
// sources know, but offer nothing to link. The rules are pure and
// unit-tested; only `decorateHandles` touches a DOM.

import { iconUrl } from "@/config/icons";
import { STEP_GLYPHS, glyphSvg } from "@/config/glyphs";
import { filterToken } from "@/grid/query";
import { uriFromHandle } from "./chipLinks";
import { Resolver } from "./resolver";
import { pushToast } from "@/toasts";
import { UNIFIED_INDEX } from "@/api";

export const CONTACTS_APPLET = "/applet/datalib_contacts";

/** `datalib_contact_schema::NormalizedContact`, hand-kept: a person as one
 *  source describes them. */
export type NormalizedContact = {
  source_id: string;
  key: string;
  kind: "person" | "group";
  names: string[];
  handles: {
    medium: "email" | "phone" | "other";
    label: string | null;
    value: string;
    handle: string | null;
    stopped_working_by: string | null;
  }[];
  org: string | null;
  title: string | null;
  seen: { items: number; last_at: string | null } | null;
  /** A photo the app serves, app-relative, where the source has one and
   *  the server can serve it; absent or null otherwise. The chip's lead. */
  photo_url?: string | null;
};

export type ContactSummary = { contact_id: string; name: string; kind: string };

/** For one handle: the contact a person made, if any, and every source's
 *  record of whoever holds it, ranked. */
export type Who = { mine: NormalizedContact | null; sourceContacts: NormalizedContact[] };

// ── Pure rules ─────────────────────────────────────────────────────────

export type HandleKind = "email" | "tel" | "slack" | "signal_aci";

export function handleKind(handle: string): HandleKind | null {
  const kind = handle.slice(0, handle.indexOf(":"));
  return kind === "email" || kind === "tel" || kind === "slack" || kind === "signal_aci"
    ? kind
    : null;
}

export function handleValue(handle: string): string {
  return handle.slice(handle.indexOf(":") + 1);
}

const KIND_ICON: Record<HandleKind, string> = {
  email: "email",
  tel: "sms",
  slack: "slack",
  signal_aci: "signal",
};

export function handleIcon(handle: string): string | null {
  const kind = handleKind(handle);
  return kind ? KIND_ICON[kind] : null;
}

/** A name to offer for a new contact, from what the source showed:
 *  `Will Riker <riker@enterprise.org>` → `Will Riker`. Nothing when the
 *  source showed only the identifier itself. */
export function suggestedName(shownAs: string, handle: string): string {
  const name = shownAs
    .replace(/\s*<[^>]*>\s*$/, "")
    .replace(/^"(.*)"$/, "$1")
    .trim();
  return name === handleValue(handle) || name.includes("@") ? "" : name;
}

export function todayPartialDate(now: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

/** What the source called the handle, without the identifier it may
 *  have carried: `Will Riker <riker@enterprise.org>` → `Will Riker`. The
 *  identifier itself when that is all the source showed. */
export function sourceLabel(shownAs: string, handle: string): string {
  return suggestedName(shownAs, handle) || shownAs.trim() || handleValue(handle);
}

export function nameOf(c: NormalizedContact): string {
  return c.names[0] ?? c.key;
}

/** A partial date by which `handle` had stopped working, as `c` records it. */
export function stoppedBy(c: NormalizedContact | null, handle: string): string | null {
  return c?.handles.find((h) => h.handle === handle)?.stopped_working_by ?? null;
}

export type ChipLook = {
  text: string;
  ariaLabel: string;
  classes: string[];
  /** A contact's initial, drawn in a small disc; null for an
   *  unresolved handle, which shows its kind's mark instead. */
  initial: string | null;
  icon: string | null;
  /** The person's photo, which leads in place of the initial or the
   *  mark: your contact's, else the best a source gave. */
  photo: string | null;
  /** The tooltip: who, the identifier, and what each source knows. */
  title: string;
};

/** The photo to lead with: your contact's, else the first source's. */
function photoOf(who: Who): string | null {
  return who.mine?.photo_url ?? who.sourceContacts.find((a) => a.photo_url)?.photo_url ?? null;
}

/** `canLink` is whether a contacts app is there to link the handle with. */
export function chipLook(handle: string, shownAs: string, who: Who, canLink: boolean): ChipLook {
  const { mine, sourceContacts } = who;
  if (!mine) {
    const text = sourceContacts[0] ? nameOf(sourceContacts[0]) : sourceLabel(shownAs, handle);
    return {
      text,
      ariaLabel: `${text}, ${handleValue(handle)}, not linked to a contact`,
      classes: ["handle-chip", "handle-unresolved", ...(canLink ? ["handle-linkable"] : [])],
      initial: null,
      icon: handleIcon(handle),
      photo: photoOf(who),
      title: chipTooltip(handle, shownAs, who, canLink),
    };
  }
  const name = nameOf(mine);
  return {
    text: name,
    ariaLabel: `${name}, ${handleValue(handle)}`,
    classes: [
      "handle-chip",
      "handle-resolved",
      ...(stoppedBy(mine, handle) ? ["handle-stale"] : []),
    ],
    initial: [...name.trim()][0]?.toUpperCase() ?? "?",
    icon: null,
    photo: photoOf(who),
    title: chipTooltip(handle, shownAs, who, canLink),
  };
}

/** What a right-click on a chip offers. An entry is an *id* the surface
 *  binds a handler to — the document view and a grid cell draw the same
 *  menu and act on it their own way (docs/dev/plans/chips.md § Clicks). */
export type ChipMenuId = "copy-name" | "copy-id" | "copy-both" | "search" | "edit";
export type ChipMenuEntry = { id: ChipMenuId; label: string; separator?: boolean };

/** The menu for one chip: copy its name, its identifier, or both; find
 *  everything from this person; and, with a contacts app, link or edit
 *  the link. `canLink` is whether a contacts app is there to link with. */
export function chipMenu(
  handle: string,
  shownAs: string,
  who: Who,
  canLink: boolean,
): ChipMenuEntry[] {
  const { text: name } = chipLook(handle, shownAs, who, canLink);
  const value = handleValue(handle);
  const named = name !== value;
  const out: ChipMenuEntry[] = [];
  if (named) out.push({ id: "copy-name", label: `Copy “${name}”` });
  out.push({ id: "copy-id", label: `Copy ${value}` });
  if (named) out.push({ id: "copy-both", label: `Copy “${copyText(handle, name)}”` });
  out.push({ id: "search", label: `Everything from ${name}`, separator: true });
  if (canLink) {
    out.push({
      id: "edit",
      label: who.mine ? "Edit contact link…" : "Link to a contact…",
      separator: true,
    });
  }
  return out;
}

/** The search that finds everything from this person. The grid's Author
 *  column is the author as shown, so the term is the name the chip shows;
 *  once `grid_rows` carries `author_handle` (chips.md, step 4) this
 *  becomes a term on the handle itself. */
export function searchQueryFor(handle: string, shownAs: string, who: Who): string {
  return filterToken("author", chipLook(handle, shownAs, who, false).text, false);
}

const MAX_SOURCE_CONTACTS = 4;
const MAX_OTHER_HANDLES = 4;

export function chipTooltip(handle: string, shownAs: string, who: Who, canLink: boolean): string {
  const { mine, sourceContacts } = who;
  const name = mine
    ? nameOf(mine)
    : sourceContacts[0]
      ? nameOf(sourceContacts[0])
      : sourceLabel(shownAs, handle);
  const value = handleValue(handle);
  const lines: string[] = value === name ? [name] : [name, value];
  const stopped = stoppedBy(mine, handle);
  if (stopped) lines.push(`Stopped working by ${stopped}`);
  if (shownAs.trim() && shownAs.trim() !== name) lines.push(`Shown here as “${shownAs.trim()}”`);
  for (const a of sourceContacts.slice(0, MAX_SOURCE_CONTACTS)) {
    const items = a.seen ? ` · ${a.seen.items} ${a.seen.items === 1 ? "item" : "items"}` : "";
    lines.push(`${nameOf(a)} in ${a.source_id}${items}`);
  }
  const others = [
    ...new Set(
      [mine, ...sourceContacts]
        .flatMap((c) => c?.handles ?? [])
        .filter((h) => h.handle !== handle)
        // An address or a number reads as itself; a Slack user's
        // `T…/U…` needs its kind beside it.
        .map((h) => (h.medium === "other" && h.handle ? h.handle : h.value)),
    ),
  ];
  if (others.length) lines.push(`Also ${others.slice(0, MAX_OTHER_HANDLES).join(", ")}`);
  if (mine) lines.push("Click to edit");
  else if (canLink) lines.push("Not linked to a contact. Click to link it.");
  return lines.join("\n");
}

/** A chip as copied text: the name it shows and the identifier behind
 *  it, so a paste loses neither. */
export function copyText(handle: string, label: string): string {
  const value = handleValue(handle);
  if (!label || label === value) return value;
  switch (handleKind(handle)) {
    case "email":
      return `${label} <${value}>`;
    case "tel":
      return `${label} (${value})`;
    default:
      return `${label} (${handle})`;
  }
}

/** Replace every chip in a copied fragment with the link it was written
 *  as — `Name <identifier>` as text, the handle's URI as href, the
 *  description as title — so a paste keeps a working link and a paste
 *  back into the app chips it again. Mutates `fragment`; returns whether
 *  it held any chip. */
export function rewriteChipsForCopy(fragment: DocumentFragment | Element): boolean {
  // A group or step chip copies as its name and its URI, the link it was.
  const named = Array.from(fragment.querySelectorAll<HTMLElement>("a.chip[data-entity]"));
  for (const chip of named) {
    const uri = chip.dataset.entity ?? "";
    const text = `${chip.dataset.label ?? chip.textContent ?? ""} (${uri})`;
    const a = chip.ownerDocument.createElement("a");
    a.href = uri;
    a.dataset.entity = uri;
    a.title = text;
    a.textContent = text;
    chip.replaceWith(a);
  }
  const chips = Array.from(fragment.querySelectorAll<HTMLElement>(".handle-chip[data-handle]"));
  for (const chip of chips) {
    const handle = chip.dataset.handle ?? "";
    const text = copyText(handle, chip.dataset.label ?? "");
    const a = chip.ownerDocument.createElement("a");
    a.dataset.handle = handle;
    const uri = uriFromHandle(handle);
    if (uri) a.href = uri;
    a.title = text;
    a.textContent = text;
    chip.replaceWith(a);
  }
  return chips.length + named.length > 0;
}

/** Every chip link under `root`: the links `chipLinks.js` marked because
 *  their href names a handle. Anywhere in the body counts, a mention as
 *  much as the header: a chip shows who the handle resolves to, never
 *  the link text, so a link a sender wrote can only point at a real
 *  person under their real name (docs/dev/plans/chips.md § Trust). */
export function chipAnchors(root: Element): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-handle]"));
}

// ── The applet ────────────────────────────────────────────────────────

/** Whether a failed call means the app is simply not configured: the
 *  gateway's 502 names the applet it has no entry for. */
export function isAbsent(status: number, body: string): boolean {
  if (status !== 502) return false;
  try {
    return (JSON.parse(body) as { error?: string }).error === 'no applet "datalib_contacts"';
  } catch {
    return false;
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await fetch(`${CONTACTS_APPLET}${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json" } : undefined,
  });
  const text = await r.text();
  if (!r.ok) {
    let msg = text;
    try {
      msg = (JSON.parse(text) as { error?: string }).error ?? text;
    } catch {
      // not JSON: the text itself is the message
    }
    throw new Error(msg);
  }
  return JSON.parse(text) as T;
}

const post = <T>(path: string, body: unknown) =>
  call<T>(path, { method: "POST", body: JSON.stringify(body) });

/** `null` when no contacts app is configured. */
export async function resolveHandles(
  handles: string[],
): Promise<Record<string, NormalizedContact> | null> {
  const r = await fetch(`${CONTACTS_APPLET}/resolve`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ handles }),
  });
  const text = await r.text();
  if (isAbsent(r.status, text)) return null;
  if (!r.ok) throw new Error(`resolve → ${r.status}: ${text}`);
  return (JSON.parse(text) as { resolved: Record<string, NormalizedContact> }).resolved;
}

/** Every source's record of whoever holds each handle, ranked; a handle
 *  no source mentions is absent. */
export async function peopleFor(handles: string[]): Promise<Record<string, NormalizedContact[]>> {
  const r = await fetch(`${UNIFIED_INDEX}/people`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ handles }),
  });
  if (!r.ok) throw new Error(`people → ${r.status}: ${await r.text()}`);
  return ((await r.json()) as { people: Record<string, NormalizedContact[]> }).people;
}

export async function searchContacts(q: string): Promise<ContactSummary[]> {
  const r = await call<{ contacts: ContactSummary[] }>(`/search?q=${encodeURIComponent(q)}`);
  return r.contacts;
}

// Every edit forgets the handles it touched, so every document and grid
// showing them draws them again from the next answer.

export async function createContact(name: string, handles: string[]): Promise<string> {
  const id = (await post<{ contact_id: string }>("/contacts", { name, handles })).contact_id;
  people.forget(handles);
  return id;
}

export async function linkHandle(handle: string, contactId: string): Promise<void> {
  await post("/link", { handle, contact_id: contactId });
  people.forget([handle]);
}

export async function unlinkHandle(handle: string): Promise<void> {
  await post("/unlink", { handle });
  people.forget([handle]);
}

export async function setStoppedWorking(handle: string, by: string | null): Promise<void> {
  await post("/stopped_working", { handle, by });
  people.forget([handle]);
}

// ── Who a handle is, for the whole app ────────────────────────────────

export const NOBODY: Who = { mine: null, sourceContacts: [] };

let contactsApp = false;

/** Who each handle is: the contact a person made, if any, and every
 *  source's record of them. One resolver for every document and grid
 *  (`resolver.ts`). */
export const people = new Resolver<Who>(
  async (handles) => {
    const [mine, sourceContacts] = await Promise.all([resolveHandles(handles), peopleFor(handles)]);
    contactsApp = mine !== null;
    return new Map(
      handles.map((h) => [h, { mine: mine?.[h] ?? null, sourceContacts: sourceContacts[h] ?? [] }]),
    );
  },
  // The toast dedupes itself, so a page of chips failing says so once.
  (e) => pushToast(`Contacts: ${e.message}`),
);

/** Whether a contacts app answered the last question, so a chip can offer
 *  to link a handle. */
export function canLinkHandles(): boolean {
  return contactsApp;
}

// ── The DOM ───────────────────────────────────────────────────────────

/** Ask who every chip link under `root` is and draw it as a chip, from
 *  `people`. Safe to call again — after an edit, or when `people` says an
 *  answer changed: each link keeps what the source showed in
 *  `data-shown-as`. A handle whose question failed is drawn unresolved. */
export async function decorateHandles(root: HTMLElement): Promise<void> {
  const spans = chipAnchors(root);
  if (spans.length === 0) return;
  for (const s of spans) {
    if (s.dataset.shownAs === undefined) s.dataset.shownAs = s.textContent ?? "";
  }
  await people.ask(new Set(spans.map((s) => s.dataset.handle ?? "")));
  const canLink = canLinkHandles();
  for (const s of spans) {
    if (!s.isConnected) continue;
    const handle = s.dataset.handle ?? "";
    drawChip(s, chipLook(handle, s.dataset.shownAs ?? "", people.get(handle) ?? NOBODY, canLink));
  }
}

/** Draw `look` onto a chip link: the lead (an initial in a disc, or the
 *  kind's mark), then the name, and the tooltip. The link's own class
 *  stays; a redraw replaces only what the chip added. */
export function drawChip(el: HTMLElement, look: ChipLook): void {
  el.dataset.baseClass ??= el.className;
  el.className = [el.dataset.baseClass, ...look.classes].join(" ");
  el.title = look.title;
  el.setAttribute("aria-label", look.ariaLabel);
  el.dataset.label = look.text;
  const lead = chipLead(el.ownerDocument, look);
  if (look.photo && lead) {
    // A photo the browser cannot draw (a type it does not decode, a blob
    // gone since the render) gives way to what the chip draws without one.
    lead.addEventListener("error", () => drawChip(el, { ...look, photo: null }), { once: true });
  }
  const text = el.ownerDocument.createTextNode(look.text);
  if (lead) el.replaceChildren(lead, text);
  else el.replaceChildren(text);
}

/// The chip's lead: the photo, else the initial in a disc, else the
/// mark its icon token names — a bundled picture, or a pipeline glyph
/// for a step. Decoration: `aria-hidden`, and not copied.
function chipLead(doc: Document, look: ChipLook): Element | null {
  if (look.photo) {
    const img = doc.createElement("img");
    img.src = look.photo;
    img.alt = "";
    img.className = "handle-photo";
    img.setAttribute("aria-hidden", "true");
    return img;
  }
  if (look.initial) {
    const disc = doc.createElement("span");
    disc.className = "handle-initial";
    disc.textContent = look.initial;
    disc.setAttribute("aria-hidden", "true");
    return disc;
  }
  const url = iconUrl(look.icon);
  if (url) {
    const img = doc.createElement("img");
    img.src = url;
    img.alt = "";
    img.className = "handle-mark";
    img.setAttribute("aria-hidden", "true");
    return img;
  }
  const glyph =
    look.icon === "applet"
      ? STEP_GLYPHS.applet
      : look.icon?.startsWith("step:")
        ? STEP_GLYPHS[look.icon.slice(5) as keyof typeof STEP_GLYPHS]
        : undefined;
  if (!glyph) return null;
  const mark = doc.createElement("span");
  mark.className = "handle-mark handle-glyph";
  mark.setAttribute("aria-hidden", "true");
  mark.append(doc.importNode(glyphSvg(glyph, "", 12), true));
  return mark;
}

/** A chip for a grid cell: the same link a renderer writes, drawn at
 *  once from what is known. `who` is undefined until the grid has asked. */
export function chipCell(
  handle: string,
  shownAs: string,
  who: Who | undefined,
  canLink: boolean,
): HTMLAnchorElement {
  const a = document.createElement("a");
  a.className = "chip";
  const uri = uriFromHandle(handle);
  if (uri) a.href = uri;
  a.dataset.handle = handle;
  a.dataset.shownAs = shownAs;
  drawChip(a, chipLook(handle, shownAs, who ?? { mine: null, sourceContacts: [] }, canLink));
  return a;
}

/** The selection, as a range inside `root`, or null when it is elsewhere.
 *  The body is drawn in the document frame (`docFrame.ts`), whose own
 *  document holds the selection. */
function selectionWithin(root: HTMLElement): Range | null {
  const sel = root.ownerDocument.getSelection();
  if (!sel || sel.rangeCount === 0 || sel.isCollapsed) return null;
  const range = sel.getRangeAt(0);
  return root.contains(range.commonAncestorContainer) ? range : null;
}

/** A copy from the document: chips become `Name <identifier>` in the
 *  plain text and a link with the handle's URI in the HTML. A selection
 *  with no chip in it is left to the browser. */
export function copyWithHandles(ev: ClipboardEvent, root: HTMLElement): void {
  const range = selectionWithin(root);
  if (!range || !ev.clipboardData) return;
  const fragment = range.cloneContents();
  if (!rewriteChipsForCopy(fragment)) return;
  // `innerText` keeps line breaks only for a laid-out element, so the
  // copy is laid out off-screen for the moment it is read.
  const holder = root.ownerDocument.createElement("div");
  holder.style.cssText = "position:fixed;left:-99999px;top:0;width:800px";
  holder.append(fragment);
  root.append(holder);
  const text = holder.innerText;
  const html = holder.innerHTML;
  holder.remove();
  ev.clipboardData.setData("text/plain", text);
  ev.clipboardData.setData("text/html", html);
  ev.preventDefault();
}
