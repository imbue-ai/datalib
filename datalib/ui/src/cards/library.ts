// The Library card's decisions, apart from what draws them: what a level
// of the tree holds, how it is ordered, what a node opens and what its card
// searches (cards/LibraryCard.ce.vue, cards/libs/nodeView.ts).
import type { ManageRow, Place } from "@/api";
import { filterToken } from "@/grid/query";
import { nodeSource, type NodeViewOpts } from "./cardSources";
import { uriFromEntity } from "./chipLinks";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/// A group id, as the config loader takes one: path segments of the
/// portable filename characters, none of them `.` or `..`.
const GROUP_ID =
  /^(?!\.{1,2}(?:\/|$))[A-Za-z0-9_.][A-Za-z0-9_.-]*(?:\/(?!\.{1,2}(?:\/|$))[A-Za-z0-9_.][A-Za-z0-9_.-]*)*$/;

/// Whether `node` is what a node card takes: a node's uuid, or a source's
/// top. A card's arguments come from whoever wrote its source, and this
/// one goes into a request's path.
export function isNodeId(node: unknown): node is string {
  if (typeof node !== "string") return false;
  const group = node.startsWith("source:") ? node.slice("source:".length) : null;
  return group === null ? UUID.test(node) : GROUP_ID.test(group);
}

/// What a source holds at its top, as `/places` names it.
export function sourceTop(groupId: string): string {
  return `source:${groupId}`;
}

/// One node of the tree, as `<vs-tree>` draws it.
export type TreeNode = {
  id: string;
  label: string;
  /// An image's URL, drawn before the label: a source's mark.
  icon?: string;
  kind?: string;
  description?: string;
  hasChildren: boolean;
  /// The document the place is, when it is one: a Notion page, a contact
  /// group's card.
  doc?: string;
  /// A source's mark, by its icon name: what its chip leads with until the
  /// server says what the source is now.
  mark?: string;
};

/// The library's top: each configured source, in the config's order, as a
/// node whose children are the places it holds at its top, opening only
/// where `holdsPlaces` says it holds some. A source is drawn as its
/// group's chip, led by the mark its Manage row leads with. A group with no type
/// is not a source (the index's own group), nor is a comparison of two of
/// a source's commits (a `diff` group): neither holds places.
export function sourceNodes(
  rows: ManageRow[],
  holdsPlaces: (id: string) => boolean = () => true,
): TreeNode[] {
  return rows
    .filter((r) => r.kind === "group" && r.type !== null && r.type.id !== "diff")
    .map((r) => {
      const mark = r.name.icon ?? r.type?.icon;
      return {
        id: sourceTop(r.id),
        label: r.name.label,
        ...(mark ? { mark } : {}),
        hasChildren: holdsPlaces(sourceTop(r.id)),
      };
    });
}

/// The chip a node of the tree is drawn as: a source as its group's
/// (docs/dev/chips.md § "A group or a step"), any other node as its own
/// (§ "A node"). Null for an id of neither shape, which draws as text.
export type TreeChip =
  | { kind: "group"; uri: string; shown: string; mark: string | null }
  | { kind: "node"; id: string; name: string; nodeKind: string; doc: string | null };

export function treeChip(node: TreeNode): TreeChip | null {
  if (!isNodeId(node.id)) return null;
  if (node.id.startsWith("source:")) {
    const group = node.id.slice("source:".length);
    return {
      kind: "group",
      uri: uriFromEntity("group", group),
      shown: node.label,
      mark: node.mark ?? null,
    };
  }
  return {
    kind: "node",
    id: node.id,
    name: node.label,
    nodeKind: node.kind ?? "",
    doc: node.doc ?? null,
  };
}

/// What each way of picking a node in the tree does: `pane` shows it
/// beside the tree, `card` opens its chip's card, as a chip's double-click
/// does everywhere. One table, so it can become a setting.
export const TREE_PICKS = {
  select: "pane",
  enter: "pane",
  doubleClickOnChip: "card",
  doubleClickElsewhere: "pane",
} as const satisfies Record<string, "pane" | "card">;

/// A level of places, as `/places` listed it: in the
/// source's own order, and by name where the source gives none (a
/// mailbox's labels, a workspace's channels).
export function placeNodes(places: Place[]): TreeNode[] {
  return [...places]
    .sort(
      (a, b) => a.sort_key.localeCompare(b.sort_key) || placeLabel(a).localeCompare(placeLabel(b)),
    )
    .map((p) => ({
      id: p.uuid,
      label: placeLabel(p),
      kind: p.kind,
      hasChildren: p.places_below,
      ...(p.markdown_uuid ? { doc: p.markdown_uuid } : {}),
    }));
}

/// What a place is called: its name, else the start of its text, else
/// its kind.
export function placeLabel(p: Place): string {
  return p.name?.trim() || p.preview?.trim().split("\n")[0] || p.kind;
}

/// The card a node of the tree opens.
export function nodeCardSource(node: TreeNode): string {
  return nodeSource(nodeOpts(node));
}

/// What a node's card holds its search to: what is directly in a place, or
/// the whole of a source.
export function scopeOf(node: string): string {
  return node.startsWith("source:")
    ? filterToken("source", node.slice("source:".length), false)
    : filterToken("at", node, false);
}

/// What a node card shows for a node of the tree.
export function nodeOpts(node: TreeNode): NodeViewOpts {
  return { node: node.id, name: node.label, ...(node.doc ? { doc: node.doc } : {}) };
}

/// A node card's arguments, as card source gave them, or why they will
/// not do. `node` goes into a request's path and `doc` names a document
/// to read, so each is taken only in the one shape it has.
export function nodeCardProps(opts: Partial<NodeViewOpts> | undefined): {
  node: string;
  name: string | null;
  doc: string | null;
} {
  if (!isNodeId(opts?.node)) {
    throw new Error(
      'nodeView needs the node it shows, by uuid or as "source:<group id>": nodeView({ node: "<uuid>" })',
    );
  }
  const doc = opts.doc ?? null;
  if (doc !== null && !(typeof doc === "string" && UUID.test(doc))) {
    throw new Error(
      'nodeView takes the document a node is by its uuid: nodeView({ node, doc: "<uuid>" })',
    );
  }
  return { node: opts.node, name: typeof opts.name === "string" ? opts.name : null, doc };
}
