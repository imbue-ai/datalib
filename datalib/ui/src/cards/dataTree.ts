// What the library tree card (DataTreeCard.ce.vue) decides, as pure
// functions: the ids its items go by, the person's folders of sources,
// how a page of children joins what is already listed, and the state it
// keeps. The card draws and fetches; nothing here touches the DOM.
import type { ChildrenPage, TreeChild } from "@/api";

export const ROOT = "root";
const FOLDER = "folder:";
const SOURCE = "source:";
const MORE = "more:";

export type Dir = "before" | "after";

/// One item of the tree. A node is anything `/children` lists, by its uuid.
export type TreeItem =
  | { type: "root" }
  | { type: "folder"; name: string }
  | { type: "source"; sourceId: string; label: string; count: number | null }
  | { type: "node"; child: TreeChild }
  | { type: "more"; parent: string; dir: Dir }
  | { type: "loading" };

export const folderId = (key: string) => `${FOLDER}${key}`;
export const sourceItemId = (sourceId: string) => `${SOURCE}${sourceId}`;
export const moreId = (parent: string, dir: Dir) => `${MORE}${dir}:${parent}`;
export const isFolderId = (id: string) => id.startsWith(FOLDER);
export const isSourceId = (id: string) => id.startsWith(SOURCE);
export const sourceOf = (id: string) => id.slice(SOURCE.length);

export function itemName(item: TreeItem | null): string {
  if (!item) return "";
  switch (item.type) {
    case "root":
      return "Library";
    case "folder":
      return item.name;
    case "source":
      return item.label;
    case "node":
      return nodeTitle(item.child);
    case "more":
      return item.dir === "before" ? "Earlier…" : "More…";
    case "loading":
      return "Loading…";
  }
}

/// A container by its name; a row by what it says, or else who said it.
export function nodeTitle(c: TreeChild): string {
  if (c.container && c.name) return c.name;
  return c.preview?.trim() || c.name || c.author || c.kind || c.uuid;
}

export function isFolderItem(item: TreeItem | null): boolean {
  if (!item) return false;
  switch (item.type) {
    case "root":
    case "folder":
    case "source":
      return true;
    case "node":
      return item.child.container;
    default:
      return false;
  }
}

// --- The person's folders of sources ----------------------------------------

/// Which folder holds what, in order: `lists[ROOT]` and `lists[folderId]`
/// name folders and sources. A source no list names sits at the root.
export type Layout = {
  lists: Record<string, string[]>;
  names: Record<string, string>;
};

export const emptyLayout = (): Layout => ({ lists: { [ROOT]: [] }, names: {} });

/// Every folder's children, and the root's, for the sources there are:
/// a source gone from the library drops out, and a new one joins the
/// root after what the person placed, by its label.
export function layoutChildren(
  layout: Layout,
  sources: { id: string; label: string }[],
): Record<string, string[]> {
  const known = new Set(sources.map((s) => sourceItemId(s.id)));
  const keep = (id: string) => (isFolderId(id) ? id in layout.names : known.has(id));
  const out: Record<string, string[]> = { [ROOT]: [] };
  const placed = new Set<string>();
  for (const parent of [ROOT, ...Object.keys(layout.names)]) {
    const list = (layout.lists[parent] ?? []).filter((id) => keep(id) && !placed.has(id));
    list.forEach((id) => placed.add(id));
    out[parent] = list;
  }
  const loose = sources
    .filter((s) => !placed.has(sourceItemId(s.id)))
    .sort((a, b) => a.label.localeCompare(b.label))
    .map((s) => sourceItemId(s.id));
  out[ROOT] = [...out[ROOT], ...loose];
  return out;
}

/// The folders under `id`, it included: what may not take `id` as a child.
function folderAndBelow(layout: Layout, id: string): Set<string> {
  const out = new Set<string>([id]);
  const walk = (f: string) =>
    (layout.lists[f] ?? []).filter(isFolderId).forEach((c) => {
      if (!out.has(c)) {
        out.add(c);
        walk(c);
      }
    });
  walk(id);
  return out;
}

export function canMoveInto(layout: Layout, moving: string[], target: string): boolean {
  if (target !== ROOT && !isFolderId(target)) return false;
  return moving.every(
    (id) =>
      (isSourceId(id) || isFolderId(id)) &&
      !(isFolderId(id) && folderAndBelow(layout, id).has(target)),
  );
}

/// `moving` taken out of wherever they were, `shown` being what each list
/// shows now, and put in `target` at `index` (its end when undefined).
export function moveInto(
  layout: Layout,
  shown: Record<string, string[]>,
  moving: string[],
  target: string,
  index?: number,
): Layout {
  const gone = new Set(moving);
  const lists: Record<string, string[]> = {};
  for (const [parent, list] of Object.entries(shown)) {
    lists[parent] = list.filter((id) => !gone.has(id));
  }
  const into = lists[target] ?? [];
  // `index` counts the target's children as shown, the moved ones among them.
  const before = (shown[target] ?? []).slice(0, index ?? Infinity).filter((id) => !gone.has(id));
  const at = before.length;
  lists[target] = [...into.slice(0, at), ...moving, ...into.slice(at)];
  return { lists, names: layout.names };
}

export function addFolder(layout: Layout, key: string, name: string): Layout {
  const id = folderId(key);
  return {
    lists: { ...layout.lists, [ROOT]: [id, ...(layout.lists[ROOT] ?? [])], [id]: [] },
    names: { ...layout.names, [id]: name },
  };
}

export function renameFolder(layout: Layout, id: string, name: string): Layout {
  return { ...layout, names: { ...layout.names, [id]: name } };
}

/// The folder gone, what it held put where it was.
export function removeFolder(layout: Layout, id: string): Layout {
  const held = layout.lists[id] ?? [];
  const lists: Record<string, string[]> = {};
  for (const [parent, list] of Object.entries(layout.lists)) {
    if (parent === id) continue;
    lists[parent] = list.flatMap((c) => (c === id ? held : [c]));
  }
  const names = { ...layout.names };
  delete names[id];
  return { lists, names };
}

// --- Pages of children -------------------------------------------------------

/// The page a card's source asks for, within what `/children` takes.
export function pageSize(asked: unknown): number {
  const n = typeof asked === "number" && Number.isInteger(asked) ? asked : 100;
  return Math.min(Math.max(n, 1), 1000);
}

/// What a parent's children are as far as they are listed: the ids in
/// order and the cursor past each end.
export type Listed = { ids: string[]; before: string | null; after: string | null };

/// The children a parent shows: its ids, with a "more" item at each end
/// that has more past it.
export function shownChildren(parent: string, l: Listed): string[] {
  return [
    ...(l.before ? [moreId(parent, "before")] : []),
    ...l.ids,
    ...(l.after ? [moreId(parent, "after")] : []),
  ];
}

/// `page` joined to what is listed, on the side it was asked for; a child
/// listed already keeps its place.
export function joinPage(l: Listed | null, page: ChildrenPage, dir: Dir | null): Listed {
  const fresh = page.children.map((c) => c.uuid);
  if (!l || dir === null) return { ids: fresh, before: page.before, after: page.after };
  const have = new Set(l.ids);
  const added = fresh.filter((id) => !have.has(id));
  return dir === "after"
    ? { ids: [...l.ids, ...added], before: l.before, after: page.after }
    : { ids: [...added, ...l.ids], before: page.before, after: l.after };
}

// --- The sources ---------------------------------------------------------------
// Every source the grid index holds rows of, and how many. Its name is
// the chip's to resolve.

export const SOURCES_SQL = `SELECT source_id, COUNT(*) AS n FROM grid_rows
WHERE source_id IS NOT NULL GROUP BY source_id ORDER BY source_id`;

export function sourcesOf(rows: unknown[][]): { id: string; label: string; count: number }[] {
  return rows.map((r) => ({ id: String(r[0]), label: String(r[0]), count: Number(r[1]) }));
}

// --- The top of a source ------------------------------------------------------
// `/children` lists what a container holds; what sits at the top of a
// source has no container, so it is read from the grid index with `/sql`.
// The cursor is the last row's `[sort_key, uuid]`, as `/children`'s is.

export const SOURCE_TOP_SQL = `SELECT g.uuid, g.sort_key, g.kind,
  COALESCE((SELECT MAX(n.name) FROM data_nodes n WHERE n.uuid = g.uuid), g.conversation_name) AS name,
  EXISTS (SELECT 1 FROM data_nodes n WHERE n.uuid = g.uuid) AS container,
  g.author, g.created_at, g.preview, g.markdown_uuid
FROM grid_rows g
WHERE g.source_id = :source AND g.parent_uuid IS NULL AND (g.sort_key, g.uuid) > (:sort_key, :uuid)
ORDER BY g.sort_key, g.uuid
LIMIT :limit`;

export function sourceTopParams(sourceId: string, after: string | null, limit: number) {
  const [sortKey, uuid] = after ? (JSON.parse(after) as [string, string]) : ["", ""];
  // One more than a page, so a full answer says there is more.
  return { source: sourceId, sort_key: sortKey, uuid, limit: limit + 1 };
}

const str = (v: unknown) => (v === null || v === undefined ? null : String(v));

export function sourceTopPage(columns: string[], rows: unknown[][], limit: number): ChildrenPage {
  const col = (name: string) => columns.indexOf(name);
  const children: TreeChild[] = rows.slice(0, limit).map((r) => ({
    uuid: String(r[col("uuid")]),
    sort_key: String(r[col("sort_key")] ?? ""),
    kind: String(r[col("kind")] ?? ""),
    name: str(r[col("name")]),
    container: Boolean(r[col("container")]),
    author: str(r[col("author")]),
    created_at: str(r[col("created_at")]),
    preview: str(r[col("preview")]),
    markdown_uuid: str(r[col("markdown_uuid")]),
  }));
  const last = children.at(-1);
  const more = rows.length > limit && last;
  return {
    children,
    before: null,
    after: more ? JSON.stringify([last.sort_key, last.uuid]) : null,
  };
}

// --- What the card keeps -----------------------------------------------------

export type TreeState = {
  expanded: string[];
  selected: string[];
  focused: string | null;
  layout: Layout;
};

export function encodeState(s: TreeState): string {
  return JSON.stringify({ v: 1, ...s });
}

/// Best effort: anything unreadable is an empty tree with nothing open.
export function decodeState(text: string): TreeState {
  const empty: TreeState = { expanded: [], selected: [], focused: null, layout: emptyLayout() };
  if (!text) return empty;
  try {
    const raw = JSON.parse(text) as Partial<TreeState> & { v?: number };
    if (raw.v !== 1) return empty;
    const strings = (v: unknown) =>
      Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
    const lists: Record<string, string[]> = {};
    for (const [k, v] of Object.entries(raw.layout?.lists ?? {})) lists[k] = strings(v);
    const names: Record<string, string> = {};
    for (const [k, v] of Object.entries(raw.layout?.names ?? {})) {
      if (isFolderId(k) && typeof v === "string") names[k] = v;
    }
    return {
      expanded: strings(raw.expanded),
      selected: strings(raw.selected),
      focused: typeof raw.focused === "string" ? raw.focused : null,
      layout: { lists: { [ROOT]: [], ...lists }, names },
    };
  } catch {
    return empty;
  }
}
