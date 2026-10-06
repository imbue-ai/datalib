// The containers layout as a value: a tree of containers whose leaves
// are cards. Every container lays out its own children (tabs, a stack,
// a row, miller columns), and any container but the outermost can be
// solidified, which holds for everything inside it too. A card opened
// from a card lands in the nearest container above the opener that is
// not solidified, so a solidified subtree keeps its shape and an
// unsolidified one grows. The decisions are pure functions here;
// ContainersView applies them.

export const LAYOUTS = ["tabs", "page", "split", "columns"] as const;
export type Layout = (typeof LAYOUTS)[number];

// Which way a Split container shares its space.
export const DIRECTIONS = ["row", "column"] as const;
export type Direction = (typeof DIRECTIONS)[number];

export const DIRECTION_LABELS: Record<Direction, string> = {
  row: "Side by side",
  column: "Top to bottom",
};

export const DIRECTION_ICONS: Record<Direction, string> = {
  row: "M3 4h7v16H3zM14 4h7v16h-7z",
  column: "M4 3h16v7H4zM4 14h16v7H4z",
};

export const LAYOUT_LABELS: Record<Layout, string> = {
  tabs: "Tabs",
  page: "Page",
  split: "Split",
  columns: "Columns",
};

// Each layout's glyph, as stroked paths on a 24px grid.
export const LAYOUT_ICONS: Record<Layout, string> = {
  tabs: "M3 9h18v11H3zM3 9V5h8v4",
  page: "M4 3h16v4H4zM4 10h16v7H4zM4 20h16",
  split: "M3 4h18v16H3zM12 4v16",
  columns: "M3 4h18v16H3zM9 4v16M15 4v16",
};

type Common = {
  id: string;
  // The node's size along its container's axis, in px: its height in a
  // top-to-bottom split, its width in a side-by-side one or in columns. null shares what is left
  // (a column without one is DEFAULT_COLUMN px wide). A page ignores
  // it: each child there is as tall as its content.
  basis: number | null;
  // The sibling this one was opened from, so a tabs container can list
  // its children as a tree, and closing a tab closes what it opened.
  openedBy: string | null;
};

export type CardNode = Common & {
  kind: "card";
  source: string;
  // Opaque per-card state string (see HostCommands.setState).
  state: string;
  // What the card last called itself, kept so an unmounted tab has a name.
  title: string | null;
  // A name the person gave it, which the card's own title never replaces.
  name: string | null;
};

export type BoxNode = Common & {
  kind: "box";
  layout: Layout;
  // Which way a Split shares its space; kept, unused, by the others, so
  // switching back to Split finds it.
  direction: Direction;
  // This container and everything inside it keep their shape: a card
  // opened from inside lands further out, and outside edit mode none of
  // it shows chrome. A flag set further in counts again once this one
  // is turned off.
  solidified: boolean;
  children: TreeNode[];
  // The child a tabs container shows.
  selected: string | null;
  // A name the person gave it, or the composite's name.
  name: string | null;
  // The composite this was made from, so it can be reset to it.
  template: string | null;
};

export type TreeNode = CardNode | BoxNode;

export const DEFAULT_COLUMN = 640;

export function makeCard(id: string, source: string, state = ""): CardNode {
  return {
    kind: "card",
    id,
    source,
    state,
    title: null,
    name: null,
    basis: null,
    openedBy: null,
  };
}

export function makeBox(
  id: string,
  layout: Layout,
  children: TreeNode[],
  opts: Partial<Pick<BoxNode, "solidified" | "name" | "template" | "basis" | "direction">> = {},
): BoxNode {
  return {
    kind: "box",
    id,
    layout,
    direction: opts.direction ?? "row",
    children,
    solidified: opts.solidified ?? false,
    selected: layout === "tabs" ? (children[0]?.id ?? null) : null,
    name: opts.name ?? null,
    template: opts.template ?? null,
    basis: opts.basis ?? null,
    openedBy: null,
  };
}

// ---- reading ----

// The nodes from the root down to `id`, both ends included; empty when
// `id` is not in the tree.
function pathTo(root: TreeNode, id: string): TreeNode[] {
  if (root.id === id) return [root];
  if (root.kind === "box") {
    for (const child of root.children) {
      const rest = pathTo(child, id);
      if (rest.length) return [root, ...rest];
    }
  }
  return [];
}

export function find(root: TreeNode, id: string): TreeNode | undefined {
  const path = pathTo(root, id);
  return path[path.length - 1];
}

export function parentOf(root: TreeNode, id: string): BoxNode | undefined {
  const path = pathTo(root, id);
  return path.length >= 2 ? (path[path.length - 2] as BoxNode) : undefined;
}

export function cards(root: TreeNode): CardNode[] {
  return root.kind === "card" ? [root] : root.children.flatMap(cards);
}

// Whether path[k] counts as solidified: it or a container above it is.
// The outermost container never is, so an open always has somewhere to
// land.
function solidifiedAt(path: TreeNode[], k: number): boolean {
  return path.slice(1, k + 1).some((n) => n.kind === "box" && n.solidified);
}

// Whether node `id`, a card or a container, sits in a solidified subtree.
export function isSolidified(root: TreeNode, id: string): boolean {
  const path = pathTo(root, id);
  return path.length > 0 && solidifiedAt(path, path.length - 1);
}

// Where a card opened from `fromId` goes: the nearest container above
// it that is not solidified, and which of that container's children holds
// the opener.
export function landing(
  root: TreeNode,
  fromId: string,
): { boxId: string; branchId: string } | null {
  const path = pathTo(root, fromId);
  for (let k = path.length - 2; k >= 0; k--) {
    if (!solidifiedAt(path, k)) return { boxId: path[k].id, branchId: path[k + 1].id };
  }
  return null;
}

// What a tabs container lists, top to bottom: each child under the one
// it was opened from, siblings in the order they came.
export function tabRows(box: BoxNode): { node: TreeNode; depth: number }[] {
  const ids = new Set(box.children.map((c) => c.id));
  const out: { node: TreeNode; depth: number }[] = [];
  const walk = (parent: string | null, depth: number) => {
    for (const c of box.children) {
      const top = c.openedBy === null || !ids.has(c.openedBy);
      if (parent === null ? !top : c.openedBy !== parent) continue;
      out.push({ node: c, depth });
      walk(c.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
}

// ---- changing ----

// `root` with the node `id` replaced by what `fn` returns for it.
function mapNode(root: TreeNode, id: string, fn: (n: TreeNode) => TreeNode): TreeNode {
  if (root.id === id) return fn(root);
  if (root.kind === "card") return root;
  let changed = false;
  const children = root.children.map((c) => {
    const next = mapNode(c, id, fn);
    if (next !== c) changed = true;
    return next;
  });
  return changed ? { ...root, children } : root;
}

function mapBox(root: TreeNode, id: string, fn: (b: BoxNode) => BoxNode): TreeNode {
  return mapNode(root, id, (n) => (n.kind === "box" ? fn(n) : n));
}

// Show `id`: every tabs container on the way down selects the child
// that holds it.
export function reveal(root: TreeNode, id: string): TreeNode {
  const path = pathTo(root, id);
  let next = root;
  for (let k = 0; k < path.length - 1; k++) {
    const box = path[k] as BoxNode;
    if (box.layout === "tabs" && box.selected !== path[k + 1].id) {
      const childId = path[k + 1].id;
      next = mapBox(next, box.id, (b) => ({ ...b, selected: childId }));
    }
  }
  return next;
}

// Open `nodes` from `fromId` as a chain: the first opened by the
// opener's branch, each next one by the one before. Where they go
// within the landing container is the container's layout's call:
// columns drop what was right of the opener, tabs take the chain as one
// new tab — a Columns container `tabId`, so what it opens lands beside
// it — and the others insert beside it. Returns the tree unchanged
// when nothing is unsolidified above.
export function openFrom(
  root: TreeNode,
  fromId: string,
  nodes: TreeNode[],
  tabId: string,
): TreeNode {
  const land = landing(root, fromId);
  if (!land || nodes.length === 0) return root;
  const chained = nodes.map((n, i) => ({
    ...n,
    openedBy: i === 0 ? land.branchId : nodes[i - 1].id,
  }));
  const next = mapBox(root, land.boxId, (box) => {
    const i = box.children.findIndex((c) => c.id === land.branchId);
    let children: TreeNode[];
    if (box.layout === "columns") {
      children = [...box.children.slice(0, i + 1), ...chained];
    } else if (box.layout === "tabs") {
      const inner = chained.map((n, k) => ({ ...n, openedBy: k === 0 ? null : n.openedBy }));
      const tab = { ...makeBox(tabId, "columns", inner), openedBy: land.branchId };
      children = [...box.children, tab];
    } else {
      children = [...box.children.slice(0, i + 1), ...chained, ...box.children.slice(i + 1)];
    }
    return { ...box, children };
  });
  return reveal(next, nodes[nodes.length - 1].id);
}

// Append a child to a container, and show it.
export function addChild(root: TreeNode, boxId: string, node: TreeNode): TreeNode {
  const next = mapBox(root, boxId, (b) => ({ ...b, children: [...b.children, node] }));
  return reveal(next, node.id);
}

// What a tabs container closes along with a child: everything opened
// from it, all the way down.
function openedFrom(box: BoxNode, id: string): Set<string> {
  const out = new Set([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const c of box.children) {
      if (c.openedBy !== null && out.has(c.openedBy) && !out.has(c.id)) {
        out.add(c.id);
        grew = true;
      }
    }
  }
  return out;
}

// Close `id`. In a tabs container that takes what it opened with it;
// elsewhere what it opened is re-pointed at its own opener. A container
// left empty goes too; the outermost one is left empty for the caller.
export function remove(root: TreeNode, id: string): TreeNode {
  const parent = parentOf(root, id);
  if (!parent) return root;
  const gone = parent.layout === "tabs" ? openedFrom(parent, id) : new Set([id]);
  const victim = parent.children.find((c) => c.id === id)!;
  const at = parent.children.findIndex((c) => c.id === id);
  const children = parent.children
    .filter((c) => !gone.has(c.id))
    .map((c) => (c.openedBy === id ? { ...c, openedBy: victim.openedBy } : c));
  if (children.length === 0 && parent.id !== root.id) return remove(root, parent.id);
  let selected = parent.selected;
  if (selected !== null && gone.has(selected)) {
    const before = parent.children
      .slice(0, at)
      .reverse()
      .find((c) => !gone.has(c.id));
    selected = (before ?? children[0])?.id ?? null;
  }
  return mapBox(root, parent.id, (b) => ({ ...b, children, selected }));
}

export function setCard(
  root: TreeNode,
  id: string,
  patch: Partial<Pick<CardNode, "source" | "state" | "title">>,
): TreeNode {
  return mapNode(root, id, (n) => (n.kind === "card" ? { ...n, ...patch } : n));
}

export function setBasis(root: TreeNode, id: string, basis: number | null): TreeNode {
  return mapNode(root, id, (n) => ({ ...n, basis }));
}

export function setLayout(root: TreeNode, id: string, layout: Layout): TreeNode {
  return mapBox(root, id, (b) => ({
    ...b,
    layout,
    selected: layout === "tabs" ? (b.selected ?? b.children[0]?.id ?? null) : b.selected,
    // Sizes along one axis mean nothing along another.
    children: b.layout === layout ? b.children : b.children.map((c) => ({ ...c, basis: null })),
  }));
}

export function setDirection(root: TreeNode, id: string, direction: Direction): TreeNode {
  return mapBox(root, id, (b) => ({
    ...b,
    direction,
    // Sizes along one axis mean nothing along the other.
    children:
      b.direction === direction ? b.children : b.children.map((c) => ({ ...c, basis: null })),
  }));
}

// The outermost container is never solidified.
export function setSolidified(root: TreeNode, id: string, solidified: boolean): TreeNode {
  if (id === root.id) return root;
  return mapBox(root, id, (b) => ({ ...b, solidified }));
}

export function rename(root: TreeNode, id: string, name: string | null): TreeNode {
  return mapNode(root, id, (n) => ({ ...n, name }));
}

// Mark box `id` as made from the composite `template`, so it can be
// reset to it; a box with no name of its own takes the composite's.
export function setTemplate(root: TreeNode, id: string, template: string): TreeNode {
  return mapBox(root, id, (b) => ({ ...b, template, name: b.name ?? template }));
}

// Put `next` where `id` is, with its size and opener. What in the parent
// pointed at `id` — the tab it shows, the siblings opened from it —
// points at `next` instead.
function replace(root: TreeNode, id: string, next: TreeNode): TreeNode {
  const parent = parentOf(root, id);
  const old = find(root, id);
  if (!parent || !old) return root;
  const placed = { ...next, basis: old.basis, openedBy: old.openedBy };
  return mapBox(root, parent.id, (b) => ({
    ...b,
    selected: b.selected === id ? placed.id : b.selected,
    children: b.children.map((c) =>
      c.id === id ? placed : c.openedBy === id ? { ...c, openedBy: placed.id } : c,
    ),
  }));
}

// Put `id` inside a new container of its own, which takes its place.
export function wrap(root: TreeNode, id: string, layout: Layout, boxId: string): TreeNode {
  const node = find(root, id);
  if (!node) return root;
  return replace(root, id, makeBox(boxId, layout, [{ ...node, basis: null, openedBy: null }]));
}

// Replace container `id` with its children, in place.
export function unwrap(root: TreeNode, id: string): TreeNode {
  const parent = parentOf(root, id);
  const box = find(root, id);
  if (!parent || !box || box.kind !== "box") return root;
  const lifted = box.children.map((c, i) => ({
    ...c,
    basis: null,
    openedBy: i === 0 ? box.openedBy : (c.openedBy ?? box.openedBy),
  }));
  const at = parent.children.findIndex((c) => c.id === id);
  const children = [...parent.children.slice(0, at), ...lifted, ...parent.children.slice(at + 1)];
  const selected = parent.selected === id ? (lifted[0]?.id ?? null) : parent.selected;
  return mapBox(root, parent.id, (b) => ({ ...b, children, selected }));
}

// Move `id` one place earlier (-1) or later (+1) among its siblings.
export function move(root: TreeNode, id: string, delta: -1 | 1): TreeNode {
  const parent = parentOf(root, id);
  if (!parent) return root;
  const i = parent.children.findIndex((c) => c.id === id);
  const j = i + delta;
  if (j < 0 || j >= parent.children.length) return root;
  const children = [...parent.children];
  [children[i], children[j]] = [children[j], children[i]];
  return mapBox(root, parent.id, (b) => ({ ...b, children }));
}

// ---- composites ----

// A copy of `node` with fresh ids throughout, its openers and selection
// re-pointed to match: how a saved composite becomes a live one.
export function instantiate(node: TreeNode, freshId: () => string): TreeNode {
  const ids = new Map<string, string>();
  const assign = (n: TreeNode) => {
    ids.set(n.id, freshId());
    if (n.kind === "box") n.children.forEach(assign);
  };
  assign(node);
  const copy = (n: TreeNode): TreeNode => {
    const base = {
      id: ids.get(n.id)!,
      openedBy: n.openedBy !== null ? (ids.get(n.openedBy) ?? null) : null,
    };
    if (n.kind === "card") return { ...n, ...base };
    return {
      ...n,
      ...base,
      selected: n.selected !== null ? (ids.get(n.selected) ?? null) : null,
      children: n.children.map(copy),
    };
  };
  return { ...copy(node), openedBy: null };
}

// Put a fresh copy of `template` where `id` is, keeping its size and
// its place among its siblings.
export function resetTo(
  root: TreeNode,
  id: string,
  template: TreeNode,
  freshId: () => string,
): TreeNode {
  return replace(root, id, instantiate(template, freshId));
}

// ---- storing ----

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null =>
  typeof v === "number" && Number.isFinite(v) && v > 0 ? v : null;

// A node read back from storage, with every optional field filled in;
// null when what is there cannot be a node, or holds one that cannot.
function readNode(v: unknown): TreeNode | null {
  if (typeof v !== "object" || v === null) return null;
  const n = v as Record<string, unknown>;
  const id = str(n.id);
  if (id === null) return null;
  const common = { id, basis: num(n.basis), openedBy: str(n.openedBy) };
  if (n.kind === "card") {
    const source = str(n.source);
    if (source === null) return null;
    return {
      ...common,
      kind: "card",
      source,
      state: str(n.state) ?? "",
      title: str(n.title),
      name: str(n.name),
    };
  }
  const layout = LAYOUTS.find((l) => l === n.layout);
  if (n.kind !== "box" || !layout || !Array.isArray(n.children)) return null;
  const children = n.children.map(readNode);
  if (children.some((c) => c === null)) return null;
  const kids = children as TreeNode[];
  const selected = str(n.selected);
  return {
    ...common,
    kind: "box",
    layout,
    direction: DIRECTIONS.find((d) => d === n.direction) ?? "row",
    children: kids,
    solidified: n.solidified === true,
    selected: kids.some((c) => c.id === selected) ? selected : (kids[0]?.id ?? null),
    name: str(n.name),
    template: str(n.template),
  };
}

// A stored container tree, or null for anything this build cannot read:
// a tree is used whole or not at all.
export function parseTree(v: unknown): BoxNode | null {
  const node = readNode(v);
  return node?.kind === "box" ? node : null;
}

export function parseComposites(v: unknown): Record<string, BoxNode> {
  const out: Record<string, BoxNode> = {};
  if (typeof v !== "object" || v === null) return out;
  for (const [name, stored] of Object.entries(v)) {
    const node = parseTree(stored);
    if (node) out[name] = node;
  }
  return out;
}
