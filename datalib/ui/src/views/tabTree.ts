// The tabs layout's tree as a value: every tab remembers the tab that
// opened it (its parent), the way Firefox's Tree Style Tab does. The
// decisions — where a new tab goes, what closing one does, which tab is
// selected next, what the sidebar lists — are pure functions here, and
// TabsView applies them.
import type { ColumnSpec } from "@/router/columns";

export type Tab = {
  id: string;
  source: string;
  // Opaque per-card state string (see HostCommands.setState).
  state: string;
  // The tab this one was opened from; null for a root.
  parentId: string | null;
  // The title the card last set, kept so a tab that has not been
  // mounted since a reload still shows its name.
  title: string | null;
  collapsed: boolean;
  // Opened by a card and not visited since: the next card its parent
  // opens replaces it rather than piling up beside it, so clicking
  // down a grid's rows does not leave a tab per row.
  preview: boolean;
};

export type Row = { tab: Tab; depth: number; hasChildren: boolean };

export function newTab(id: string, source: string, parentId: string | null, state = ""): Tab {
  return { id, source, state, parentId, title: null, collapsed: false, preview: false };
}

export function childrenOf(tabs: Tab[], id: string | null): Tab[] {
  return tabs.filter((t) => t.parentId === id);
}

// `id` and everything below it.
export function subtree(tabs: Tab[], id: string): Set<string> {
  const out = new Set([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const t of tabs) {
      if (t.parentId !== null && out.has(t.parentId) && !out.has(t.id)) {
        out.add(t.id);
        grew = true;
      }
    }
  }
  return out;
}

// What the sidebar lists, top to bottom: depth-first, siblings in the
// order they were opened, nothing under a collapsed tab. A tab whose
// parent is gone (a hand-edited store) is listed as a root.
export function rows(tabs: Tab[]): Row[] {
  const ids = new Set(tabs.map((t) => t.id));
  const out: Row[] = [];
  const walk = (parentId: string | null, depth: number) => {
    for (const tab of tabs) {
      const isRoot = tab.parentId === null || !ids.has(tab.parentId);
      if (parentId === null ? !isRoot : tab.parentId !== parentId) continue;
      const hasChildren = tabs.some((t) => t.parentId === tab.id);
      out.push({ tab, depth, hasChildren });
      if (hasChildren && !tab.collapsed) walk(tab.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
}

// Open `sources` as a chain under `parentId`: the first is its child,
// each next one a child of the one before. The chain replaces the
// parent's preview child when that child's whole subtree is still
// preview. Returns the new list and the ids of the chain.
export function openChain(
  tabs: Tab[],
  parentId: string,
  sources: string[],
  freshId: () => string,
): { tabs: Tab[]; ids: string[] } {
  const stale = childrenOf(tabs, parentId).find((c) => c.preview);
  let doomed = new Set<string>();
  if (stale) {
    const under = subtree(tabs, stale.id);
    if (tabs.every((t) => !under.has(t.id) || t.preview)) doomed = under;
  }
  const chain: Tab[] = [];
  let prev = parentId;
  for (const source of sources) {
    const tab = { ...newTab(freshId(), source, prev), preview: true };
    chain.push(tab);
    prev = tab.id;
  }
  // The replacement takes the replaced tab's place among its siblings.
  const next: Tab[] = [];
  for (const t of tabs) {
    if (t.id === stale?.id && doomed.size > 0) next.push(...chain);
    else if (!doomed.has(t.id)) next.push(t);
  }
  if (doomed.size === 0) next.push(...chain);
  return { tabs: next, ids: chain.map((t) => t.id) };
}

// A URL of several columns (a miller link): a new root and a spine
// under it, each at the state the URL gives. Returns the new list and
// the last tab's id.
export function openStack(
  tabs: Tab[],
  specs: ColumnSpec[],
  freshId: () => string,
): { tabs: Tab[]; lastId: string } {
  const chain: Tab[] = [];
  for (const spec of specs) {
    chain.push(newTab(freshId(), spec.code, chain.at(-1)?.id ?? null, spec.state));
  }
  return { tabs: [...tabs, ...chain], lastId: chain[chain.length - 1].id };
}

// Detach a tab, with everything under it, and make it a root, listed
// right after the top-level tab it came from. A tab the person moved
// is one they mean to keep, so it is no longer a preview.
export function makeTopLevel(tabs: Tab[], id: string): Tab[] {
  const tab = tabs.find((t) => t.id === id);
  if (!tab || tab.parentId === null) return tabs;
  const byId = new Map(tabs.map((t) => [t.id, t]));
  let root = tab;
  while (root.parentId !== null && byId.has(root.parentId)) root = byId.get(root.parentId)!;
  const moved = { ...tab, parentId: null, preview: false };
  const rest = tabs.filter((t) => t.id !== id);
  const at = rest.findIndex((t) => t.id === root.id) + 1;
  return [...rest.slice(0, at), moved, ...rest.slice(at)];
}

// Close one tab. A collapsed tab takes its hidden subtree with it; an
// expanded one hands its children up to its own parent, in its place.
// Returns the new list and the ids that went.
export function closeTab(tabs: Tab[], id: string): { tabs: Tab[]; closed: Set<string> } {
  const tab = tabs.find((t) => t.id === id);
  if (!tab) return { tabs, closed: new Set() };
  if (tab.collapsed) {
    const closed = subtree(tabs, id);
    return { tabs: tabs.filter((t) => !closed.has(t.id)), closed };
  }
  const children = tabs
    .filter((t) => t.parentId === id)
    .map((t) => ({ ...t, parentId: tab.parentId }));
  const childIds = new Set(children.map((c) => c.id));
  const next: Tab[] = [];
  for (const t of tabs) {
    if (t.id === id) next.push(...children);
    else if (!childIds.has(t.id)) next.push(t);
  }
  return { tabs: next, closed: new Set([id]) };
}

// Which tab to show after `closed` went: the opener, as Firefox does;
// failing that the row after it, then the row before it.
export function selectAfterClose(before: Tab[], after: Tab[], closedId: string): string | null {
  const alive = new Set(after.map((t) => t.id));
  const gone = before.find((t) => t.id === closedId);
  if (gone?.parentId && alive.has(gone.parentId)) return gone.parentId;
  const order = rows(before).map((r) => r.tab.id);
  const at = order.indexOf(closedId);
  const later = order.slice(at + 1).find((id) => alive.has(id));
  const earlier = order
    .slice(0, Math.max(at, 0))
    .reverse()
    .find((id) => alive.has(id));
  return later ?? earlier ?? after[0]?.id ?? null;
}

// The tab a URL names, when it names one we have: the same card at the
// same state; failing that, for a URL that carries no state (a link, a
// bookmark of a bare card), the same card at whatever state it is in —
// the selected one first.
export function tabForSpec(tabs: Tab[], selectedId: string | null, spec: ColumnSpec): Tab | null {
  const same = tabs.filter((t) => t.source === spec.code);
  const pick = (list: Tab[]) => list.find((t) => t.id === selectedId) ?? list[0] ?? null;
  const exact = pick(same.filter((t) => t.state === spec.state));
  if (exact) return exact;
  return spec.state === "" ? pick(same) : null;
}

// ---- storage ----

export type Stored = { tabs: Tab[]; selectedId: string | null };

const VERSION = 1;

export function serialize(s: Stored): string {
  return JSON.stringify({ v: VERSION, ...s });
}

// null for anything this build did not write: a person's tabs are not
// worth a crash, and a newer shape is not ours to guess at.
export function parseStored(text: string | null): Stored | null {
  if (!text) return null;
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isRecord(raw) || raw.v !== VERSION || !Array.isArray(raw.tabs)) return null;
  const tabs: Tab[] = [];
  for (const t of raw.tabs) {
    if (!isRecord(t) || typeof t.id !== "string" || typeof t.source !== "string") return null;
    tabs.push({
      id: t.id,
      source: t.source,
      state: typeof t.state === "string" ? t.state : "",
      parentId: typeof t.parentId === "string" ? t.parentId : null,
      title: typeof t.title === "string" ? t.title : null,
      collapsed: t.collapsed === true,
      preview: t.preview === true,
    });
  }
  const selectedId = typeof raw.selectedId === "string" ? raw.selectedId : null;
  return { tabs, selectedId };
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

// The tree a window starts with: its own, when a reload finds one; the
// saved one, when this is the main window starting afresh (a launch);
// and nothing — so the URL's card opens alone — for any other window.
export function startingTree(
  own: Stored | null,
  saved: Stored | null,
  mainWindow: boolean,
): Stored | null {
  if (own && own.tabs.length > 0) return own;
  if (mainWindow && saved && saved.tabs.length > 0) return saved;
  return null;
}

// The first counter value no stored id `t<n>` already uses.
export function nextCounter(tabs: Tab[]): number {
  let max = 0;
  for (const t of tabs) {
    const m = t.id.match(/^t(\d+)$/);
    if (m) max = Math.max(max, Number(m[1]));
  }
  return max + 1;
}
