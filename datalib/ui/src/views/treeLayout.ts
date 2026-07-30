// Tidy left-to-right tree layout for the tree card view (TreeView).
//
// The classic Reingold–Tilford "tidy tree" simplified for our shape:
// children sit in a column to the right of their parent and stack
// vertically, subtrees occupy disjoint vertical bands (so edges never
// cross), and a parent is vertically centered on its children's span
// — the convention mind-map tools (Miro, EdrawMind, Freeplane) use
// for programmatically spawned children. Node sizes are arbitrary;
// a child's x depends on its own parent's width, so resizing one
// node only shifts its descendants.
//
// Pure function of the node list — TreeView re-runs it on every
// open/close/resize and animates nodes to their new spots.

export type LayoutNode = {
  id: string;
  // null for roots; an id with no matching node also makes a root.
  parentId: string | null;
  width: number;
  height: number;
};

export type Rect = { x: number; y: number; width: number; height: number };

export type LayoutOpts = {
  // Horizontal gap between a parent's right edge and its children.
  hGap?: number;
  // Vertical gap between sibling subtrees (and between root trees).
  vGap?: number;
};

export function layoutTree(
  nodes: LayoutNode[],
  { hGap = 100, vGap = 32 }: LayoutOpts = {},
): Map<string, Rect> {
  const ids = new Set(nodes.map((n) => n.id));
  // Children grouped by parent, in node-list order; roots under "".
  const children = new Map<string, LayoutNode[]>();
  for (const n of nodes) {
    const key = n.parentId !== null && ids.has(n.parentId) ? n.parentId : "";
    let list = children.get(key);
    if (!list) children.set(key, (list = []));
    list.push(n);
  }
  const childrenOf = (id: string) => children.get(id) ?? [];

  // Vertical extent of each subtree: enough for the node itself or
  // for all its children's subtrees stacked with gaps, whichever is
  // taller.
  const extents = new Map<string, number>();
  function measure(n: LayoutNode): number {
    const kids = childrenOf(n.id);
    let span = 0;
    for (const k of kids) span += measure(k) + vGap;
    span -= kids.length > 0 ? vGap : 0;
    const extent = Math.max(n.height, span);
    extents.set(n.id, extent);
    return extent;
  }

  // Walk down assigning each subtree its band [top, top + extent);
  // the node and its children's stack are each centered in the band.
  const out = new Map<string, Rect>();
  function place(n: LayoutNode, x: number, top: number) {
    const extent = extents.get(n.id)!;
    out.set(n.id, {
      x,
      y: top + (extent - n.height) / 2,
      width: n.width,
      height: n.height,
    });
    const kids = childrenOf(n.id);
    if (kids.length === 0) return;
    let span = -vGap;
    for (const k of kids) span += extents.get(k.id)! + vGap;
    let y = top + (extent - span) / 2;
    for (const k of kids) {
      place(k, x + n.width + hGap, y);
      y += extents.get(k.id)! + vGap;
    }
  }

  let top = 0;
  for (const root of childrenOf("")) {
    measure(root);
    place(root, 0, top);
    top += extents.get(root.id)! + vGap;
  }
  return out;
}
