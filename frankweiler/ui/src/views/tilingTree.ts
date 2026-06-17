// Data model and pure tree operations for the tiling layout (see
// TilingView.vue). It's a small tiling-window-manager tree: every leaf
// is a *tile* holding one card, every inner node is a *split* with an
// arrangement and children. The arrangement is one of: horizontal
// (children side by side), vertical (children stacked), or tab (only
// the `active` child shown, the rest behind a tab bar).
//
// The ROOT is always a split — it starts as one horizontal node with a
// single child (one card). That single-child root is the *only*
// exception to the collapse rule below; every other split holds two or
// more children.
//
// Ways the tree changes:
//   - openCard (a card spawning another): add it as a sibling of the
//     caller — see addSibling.
//   - the "add" button at a container's end: appendChild a blank tile.
//   - drag a node onto a container's add area: moveNodeToContainer.
//   - drag a node onto a card: dropOntoLeaf — the card is replaced by a
//     new split (perpendicular to the card's parent) holding the card
//     then the dragged node.
//   - the per-container switch sets its arrangement (h/v/tab) directly
//     in TilingView; that doesn't reshape the tree.
//   - close a node: deleteNode.
//
// Collapse rule: deleting/moving a node out of a split that leaves it
// with a single child collapses that split — the lone child is
// promoted into its place, inheriting its size weight. The root never
// collapses; emptying it yields a fresh blank tile so the tree is
// never empty.
//
// Every node carries a `weight` (flex-grow relative to its siblings)
// so dividers can resize tiles; the operations here only set sensible
// defaults — TilingView mutates weights in place while dragging.

// A split's arrangement: horizontal, vertical, or tabbed. (Named
// `Dir` for the tiling cases; "tab" rides along since the host model
// is otherwise identical — only rendering and `active` differ.)
export type Dir = "h" | "v" | "tab";

export type TileLeaf = {
  kind: "leaf";
  id: string;
  // Card source — a JS expression like `gridView()`; see cards/types.ts.
  source: string;
  // Opaque per-card state string (HostCommands.setState). In memory only.
  state: string;
  // Flex-grow weight relative to siblings.
  weight: number;
};

export type TileSplit = {
  kind: "split";
  id: string;
  dir: Dir;
  weight: number;
  children: TileNode[];
  // Index of the shown child when `dir === "tab"`; ignored otherwise.
  active?: number;
};

export type TileNode = TileLeaf | TileSplit;

// Build a leaf. `weight` defaults to 1; callers pass the id so it can
// be allocated up front (openCard returns it synchronously).
export function makeTile(id: string, source: string, weight = 1): TileLeaf {
  return { kind: "leaf", id, source, state: "", weight };
}

// The starting tree: one horizontal root holding a single card.
export function makeRoot(rootId: string, child: TileNode): TileSplit {
  return { kind: "split", id: rootId, dir: "h", weight: 1, children: [child] };
}

// The perpendicular arrangement, used when a drop splits a card. Tabs
// have no axis, so default to horizontal.
export function oppositeDir(dir: Dir): "h" | "v" {
  return dir === "h" ? "v" : "h";
}

// Depth-first list of every tile in the tree, in render order. Used to
// prune the host's per-card context cache.
export function listTiles(node: TileNode): TileLeaf[] {
  if (node.kind === "leaf") return [node];
  return node.children.flatMap(listTiles);
}

// Any node (tile or split) with this id, or null.
export function findNode(node: TileNode, id: string): TileNode | null {
  if (node.id === id) return node;
  if (node.kind === "leaf") return null;
  for (const c of node.children) {
    const hit = findNode(c, id);
    if (hit) return hit;
  }
  return null;
}

// The live leaf with this id, or null. Host commands read/write a
// tile's state through this rather than capturing the object, so the
// closures keep working after an op replaces the node.
export function findTile(node: TileNode, id: string): TileLeaf | null {
  const hit = findNode(node, id);
  return hit && hit.kind === "leaf" ? hit : null;
}

// True when `id` is `node` itself or anywhere in its subtree — used to
// reject drops that would move a node into its own descendant.
export function subtreeContains(node: TileNode, id: string): boolean {
  return findNode(node, id) !== null;
}

// The split directly containing the node `id` (tile or split), or null
// when `id` is the root (or absent).
export function parentOf(node: TileNode, id: string): TileSplit | null {
  if (node.kind === "leaf") return null;
  for (const c of node.children) {
    if (c.id === id) return node;
    const deeper = parentOf(c, id);
    if (deeper) return deeper;
  }
  return null;
}

// Recompute a tab split's `active` after its children changed: keep
// the same active child where it survives (by identity), else clamp.
function fixActive(prev: TileSplit, children: TileNode[]): number {
  if (children.length === 0) return 0;
  const prevActive = prev.children[prev.active ?? 0];
  const found = prevActive ? children.indexOf(prevActive) : -1;
  return found !== -1 ? found : Math.min(prev.active ?? 0, children.length - 1);
}

function withChildren(node: TileSplit, children: TileNode[]): TileSplit {
  const next: TileSplit = { ...node, children };
  if (node.dir === "tab") next.active = fixActive(node, children);
  return next;
}

// ---- inserts ----

// Sibling: insert `newTile` immediately after the target tile within
// its parent split, reusing the parent's arrangement. The new tile
// matches the target's weight; in a tab parent it becomes active.
export function addSibling(
  node: TileNode,
  tileId: string,
  newTile: TileLeaf,
): TileNode {
  if (node.kind === "leaf") return node;
  const idx = node.children.findIndex(
    (c) => c.kind === "leaf" && c.id === tileId,
  );
  if (idx !== -1) {
    const target = node.children[idx] as TileLeaf;
    const children = node.children.slice();
    children.splice(idx + 1, 0, { ...newTile, weight: target.weight });
    const next: TileSplit = { ...node, children };
    if (node.dir === "tab") next.active = idx + 1;
    return next;
  }
  let changed = false;
  const children = node.children.map((c) => {
    const next = addSibling(c, tileId, newTile);
    if (next !== c) changed = true;
    return next;
  });
  return changed ? { ...node, children } : node;
}

// ---- add / move / drop ----

// Append `child` as the last child of the container `containerId`
// (the container's "add" end). The child's weight resets to 1; in a
// tab container it becomes the active tab.
export function appendChild(
  node: TileNode,
  containerId: string,
  child: TileNode,
): TileNode {
  if (node.kind === "leaf") return node;
  if (node.id === containerId) {
    const children = [...node.children, { ...child, weight: 1 }];
    const next: TileSplit = { ...node, children };
    // The freshly added card becomes the active tab.
    if (node.dir === "tab") next.active = children.length - 1;
    return next;
  }
  let changed = false;
  const children = node.children.map((c) => {
    const next = appendChild(c, containerId, child);
    if (next !== c) changed = true;
    return next;
  });
  return changed ? { ...node, children } : node;
}

// Move an existing direct child of `containerId` to the end of that
// container (used when a node is dragged onto its own parent's add
// area — a reorder, no detach/collapse).
function reorderToEnd(
  node: TileNode,
  containerId: string,
  childId: string,
): TileNode {
  if (node.kind === "leaf") return node;
  if (node.id === containerId) {
    const idx = node.children.findIndex((c) => c.id === childId);
    if (idx === -1 || idx === node.children.length - 1) return node;
    const children = node.children.slice();
    const [moved] = children.splice(idx, 1);
    children.push(moved);
    return withChildren(node, children);
  }
  let changed = false;
  const children = node.children.map((c) => {
    const next = reorderToEnd(c, containerId, childId);
    if (next !== c) changed = true;
    return next;
  });
  return changed ? { ...node, children } : node;
}

// Replace the leaf `leafId` with a new split (`splitId`, `dir`) holding
// the leaf then `sibling`. The split inherits the leaf's weight.
function replaceLeafWithSplit(
  node: TileNode,
  leafId: string,
  splitId: string,
  dir: Dir,
  sibling: TileNode,
): TileNode {
  if (node.kind === "leaf") {
    if (node.id !== leafId) return node;
    return {
      kind: "split",
      id: splitId,
      dir,
      weight: node.weight,
      children: [
        { ...node, weight: 1 },
        { ...sibling, weight: 1 },
      ],
    };
  }
  let changed = false;
  const children = node.children.map((c) => {
    const next = replaceLeafWithSplit(c, leafId, splitId, dir, sibling);
    if (next !== c) changed = true;
    return next;
  });
  return changed ? { ...node, children } : node;
}

// Detach the node `id` from the tree, returning the remaining tree
// (with single-child splits collapsed, the root excepted) and the
// removed node.
function detach(tree: TileNode, id: string): {
  tree: TileNode;
  node: TileNode | null;
} {
  const node = findNode(tree, id);
  if (!node) return { tree, node: null };
  return { tree: removeNode(tree, id, true) ?? tree, node };
}

// Drag a node onto a container's add area: move it to be that
// container's last child. Rejected (no-op) when the container is the
// node itself or inside it. Moving within the same parent is a plain
// reorder; otherwise detach (collapsing the old parent) then append.
export function moveNodeToContainer(
  tree: TileNode,
  draggedId: string,
  containerId: string,
): TileNode {
  const dragged = findNode(tree, draggedId);
  if (!dragged || subtreeContains(dragged, containerId)) return tree;
  const parent = parentOf(tree, draggedId);
  if (parent && parent.id === containerId) {
    return reorderToEnd(tree, containerId, draggedId);
  }
  const { tree: rest, node } = detach(tree, draggedId);
  if (!node) return tree;
  return appendChild(rest, containerId, node);
}

// Drag a node onto a card (leaf): replace the leaf with a new split
// (id `splitId`) perpendicular to the leaf's parent, holding the leaf
// then the dragged node. Rejected when dropping onto itself or a leaf
// inside the dragged node's own subtree.
export function dropOntoLeaf(
  tree: TileNode,
  draggedId: string,
  leafId: string,
  splitId: string,
): TileNode {
  if (draggedId === leafId) return tree;
  const dragged = findNode(tree, draggedId);
  if (!dragged || subtreeContains(dragged, leafId)) return tree;
  const { tree: rest, node } = detach(tree, draggedId);
  if (!node) return tree;
  const parent = parentOf(rest, leafId);
  const dir = oppositeDir(parent ? parent.dir : "h");
  return replaceLeafWithSplit(rest, leafId, splitId, dir, node);
}

// ---- delete ----

// Internal: the subtree with node `id` removed, or null if this node
// *is* the target. A non-root split left with one child collapses to
// that child (which inherits the split's weight). The root is exempt:
// it keeps its single remaining child (or none — see deleteNode).
function removeNode(
  node: TileNode,
  id: string,
  isRoot: boolean,
): TileNode | null {
  if (node.id === id) return null;
  if (node.kind === "leaf") return node;
  let changed = false;
  const children: TileNode[] = [];
  for (const c of node.children) {
    const next = removeNode(c, id, false);
    if (next !== c) changed = true;
    if (next !== null) children.push(next);
  }
  if (!changed) return node;
  if (!isRoot && children.length === 1) {
    return { ...children[0], weight: node.weight };
  }
  return withChildren(node, children);
}

// Delete a node — a tile, or an entire split subtree — by id,
// collapsing now-single-child splits (root excepted). Emptying the
// root yields a fresh blank tile so the tree is never empty.
export function deleteNode(
  node: TileNode,
  id: string,
  blank: () => TileLeaf,
): TileNode {
  const next = removeNode(node, id, true) ?? node;
  if (next.kind === "split" && next.children.length === 0) {
    return { ...next, children: [blank()] };
  }
  return next;
}
