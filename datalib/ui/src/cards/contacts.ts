// Who a handle in a document is. A renderer writes the author's handle
// on the author span (`data-handle="email:…"`); this asks the index
// (`unified_index`'s `/people`: each source's account of the person) and
// the contacts app (`datalib_contacts`: the contact a person made, ranked
// first) and turns the span into a chip. The contacts app is an app of
// its own (docs/dev/plans/contacts.md): without it chips still say what
// the sources know, but offer nothing to link. The rules are pure and
// unit-tested; only `decorateHandles` touches a DOM.

import { iconUrl } from "@/config/icons";
import { pushToast } from "@/toasts";
import { UNIFIED_INDEX } from "@/api";

export const CONTACTS_APPLET = "/applet/datalib_contacts";

/** `datalib_contact_schema::DatalibContact`, hand-kept: a person as one
 *  source describes them. */
export type DatalibContact = {
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
};

export type ContactSummary = { contact_id: string; name: string; kind: string };

/** For one handle: the contact a person made, if any, and every source's
 *  account of whoever holds it, ranked. */
export type Who = { mine: DatalibContact | null; accounts: DatalibContact[] };

// ── Pure rules ─────────────────────────────────────────────────────────

export type HandleKind = "email" | "tel" | "slack";

export function handleKind(handle: string): HandleKind | null {
  const kind = handle.slice(0, handle.indexOf(":"));
  return kind === "email" || kind === "tel" || kind === "slack" ? kind : null;
}

export function handleValue(handle: string): string {
  return handle.slice(handle.indexOf(":") + 1);
}

const KIND_ICON: Record<HandleKind, string> = { email: "email", tel: "sms", slack: "slack" };

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

export function nameOf(c: DatalibContact): string {
  return c.names[0] ?? c.key;
}

/** A partial date by which `handle` had stopped working, as `c` records it. */
export function stoppedBy(c: DatalibContact | null, handle: string): string | null {
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
};

/** `canLink` is whether a contacts app is there to link the handle with. */
export function chipLook(handle: string, shownAs: string, who: Who, canLink: boolean): ChipLook {
  const { mine, accounts } = who;
  if (!mine) {
    const text = accounts[0] ? nameOf(accounts[0]) : sourceLabel(shownAs, handle);
    return {
      text,
      ariaLabel: `${text}, ${handleValue(handle)}, not linked to a contact`,
      classes: ["handle-chip", "handle-unresolved", ...(canLink ? ["handle-linkable"] : [])],
      initial: null,
      icon: handleIcon(handle),
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
  };
}

export type HoverCard = {
  name: string;
  /** The handle as a person reads it, beside its kind's mark. */
  value: string;
  icon: string | null;
  lines: string[];
};

const MAX_ACCOUNTS = 4;
const MAX_OTHER_HANDLES = 4;

export function hoverCard(handle: string, shownAs: string, who: Who, canLink: boolean): HoverCard {
  const { mine, accounts } = who;
  const name = mine
    ? nameOf(mine)
    : accounts[0]
      ? nameOf(accounts[0])
      : sourceLabel(shownAs, handle);
  const lines: string[] = [];
  const stopped = stoppedBy(mine, handle);
  if (stopped) lines.push(`Stopped working by ${stopped}`);
  if (shownAs.trim() && shownAs.trim() !== name) lines.push(`Shown here as “${shownAs.trim()}”`);
  for (const a of accounts.slice(0, MAX_ACCOUNTS)) {
    const items = a.seen ? ` · ${a.seen.items} ${a.seen.items === 1 ? "item" : "items"}` : "";
    lines.push(`${nameOf(a)} in ${a.source_id}${items}`);
  }
  const others = [
    ...new Set(
      [mine, ...accounts]
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
  return { name, value: handleValue(handle), icon: handleIcon(handle), lines };
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

/** Replace every chip in a copied fragment with its copy text, keeping
 *  `data-handle` on a plain span so a paste into the app can chip it
 *  again. Mutates `fragment`; returns whether it held any chip. */
export function rewriteChipsForCopy(fragment: DocumentFragment | Element): boolean {
  const chips = Array.from(fragment.querySelectorAll<HTMLElement>(".handle-chip[data-handle]"));
  for (const chip of chips) {
    const handle = chip.dataset.handle ?? "";
    const span = chip.ownerDocument.createElement("span");
    span.dataset.handle = handle;
    span.textContent = copyText(handle, chip.dataset.label ?? "");
    chip.replaceWith(span);
  }
  return chips.length > 0;
}

/** The handle spans a renderer wrote, and none a message body did.
 *  DOMPurify keeps every `data-*`, and a body is HTML a stranger wrote,
 *  so a `data-handle` counts only in a top-level `.msg`'s first `h2` —
 *  its header — on the `.msg-author`, and on the `.msg-recipient`s of a
 *  `.msg-recipients` line that is the header's very next element. The
 *  renderer writes both before the body, which cannot precede them. */
export function trustedHandleSpans(root: Element): HTMLElement[] {
  const out: HTMLElement[] = [];
  for (const msg of root.querySelectorAll<HTMLElement>(".msg[data-section-uuid]")) {
    if (msg.parentElement?.closest(".msg")) continue;
    const header = Array.from(msg.children).find((c) => c.tagName === "H2");
    if (!header) continue;
    const author = header.querySelector<HTMLElement>(":scope > span.msg-author[data-handle]");
    if (author) out.push(author);
    const next = header.nextElementSibling;
    if (next?.matches("div.msg-recipients")) {
      out.push(...next.querySelectorAll<HTMLElement>(":scope > span.msg-recipient[data-handle]"));
    }
  }
  return out;
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
): Promise<Record<string, DatalibContact> | null> {
  const r = await fetch(`${CONTACTS_APPLET}/resolve`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ handles }),
  });
  const text = await r.text();
  if (isAbsent(r.status, text)) return null;
  if (!r.ok) throw new Error(`resolve → ${r.status}: ${text}`);
  return (JSON.parse(text) as { resolved: Record<string, DatalibContact> }).resolved;
}

/** Every source's account of whoever holds each handle, ranked; a handle
 *  no source mentions is absent. */
export async function peopleFor(handles: string[]): Promise<Record<string, DatalibContact[]>> {
  const r = await fetch(`${UNIFIED_INDEX}/people`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ handles }),
  });
  if (!r.ok) throw new Error(`people → ${r.status}: ${await r.text()}`);
  return ((await r.json()) as { people: Record<string, DatalibContact[]> }).people;
}

export async function searchContacts(q: string): Promise<ContactSummary[]> {
  const r = await call<{ contacts: ContactSummary[] }>(`/search?q=${encodeURIComponent(q)}`);
  return r.contacts;
}

export async function createContact(name: string, handles: string[]): Promise<string> {
  return (await post<{ contact_id: string }>("/contacts", { name, handles })).contact_id;
}

export async function linkHandle(handle: string, contactId: string): Promise<void> {
  await post("/link", { handle, contact_id: contactId });
}

export async function unlinkHandle(handle: string): Promise<void> {
  await post("/unlink", { handle });
}

export async function setStoppedWorking(handle: string, by: string | null): Promise<void> {
  await post("/stopped_working", { handle, by });
}

// ── The DOM ───────────────────────────────────────────────────────────

let warnedOnce = false;

export type Decorated = { who: Record<string, Who>; canLink: boolean };

/** Ask who every trusted handle span under `root` is and draw it as a
 *  chip; `null` when there is nothing to draw. Safe to call again after
 *  an edit: each span keeps what the source showed in `data-shown-as`. */
export async function decorateHandles(root: HTMLElement): Promise<Decorated | null> {
  const spans = trustedHandleSpans(root);
  if (spans.length === 0) return null;
  for (const s of spans) {
    if (s.dataset.shownAs === undefined) s.dataset.shownAs = s.textContent ?? "";
  }
  const handles = [...new Set(spans.map((s) => s.dataset.handle ?? ""))];
  let mine: Record<string, DatalibContact> | null;
  let people: Record<string, DatalibContact[]>;
  try {
    [mine, people] = await Promise.all([resolveHandles(handles), peopleFor(handles)]);
  } catch (e) {
    if (!warnedOnce) pushToast(`Contacts: ${(e as Error).message}`);
    warnedOnce = true;
    return null;
  }
  const canLink = mine !== null;
  const who: Record<string, Who> = Object.fromEntries(
    handles.map((h) => [h, { mine: mine?.[h] ?? null, accounts: people[h] ?? [] }]),
  );
  for (const s of spans) {
    if (!s.isConnected) continue;
    const handle = s.dataset.handle ?? "";
    const look = chipLook(handle, s.dataset.shownAs ?? "", who[handle], canLink);
    // The span's own class — an author's or a recipient's — stays; a
    // redraw replaces only what the chip added.
    s.dataset.baseClass ??= s.className;
    s.className = [s.dataset.baseClass, ...look.classes].join(" ");
    s.removeAttribute("title");
    s.setAttribute("aria-label", look.ariaLabel);
    s.dataset.label = look.text;
    const lead = s.ownerDocument.createElement(look.initial ? "span" : "img");
    lead.setAttribute("aria-hidden", "true");
    if (look.initial) {
      lead.className = "handle-initial";
      lead.textContent = look.initial;
    } else {
      const url = iconUrl(look.icon);
      if (url) (lead as HTMLImageElement).src = url;
      (lead as HTMLImageElement).alt = "";
      lead.className = "handle-mark";
    }
    s.replaceChildren(lead, s.ownerDocument.createTextNode(look.text));
  }
  return { who, canLink };
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
 *  plain text and keep their `data-handle` in the HTML. A selection with
 *  no chip in it is left to the browser. */
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
