// The Library card's decisions, apart from what draws them: what a level
// of the tree holds, how it is ordered, what a node opens, and how a node
// card lists a container's children (cards/LibraryCard.ce.vue,
// cards/NodeCard.ce.vue).
import type { Child, ManageRow } from "@/api";
import { documentSource, nodeSource, type NodeViewOpts } from "./cardSources";
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

/// What a source holds at its top, as `/children` names it.
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

/// A level of places, as `/children?places=true` listed it: in the
/// source's own order, and by name where the source gives none (a
/// mailbox's labels, a workspace's channels).
export function placeNodes(children: Child[]): TreeNode[] {
  return [...children]
    .sort(
      (a, b) => a.sort_key.localeCompare(b.sort_key) || childLabel(a).localeCompare(childLabel(b)),
    )
    .map((c) => ({
      id: c.uuid,
      label: childLabel(c),
      kind: c.kind,
      hasChildren: c.places_below === true,
      ...(c.markdown_uuid ? { doc: c.markdown_uuid } : {}),
    }));
}

/// What a child is called: its name, else the start of its text, else
/// its kind.
export function childLabel(c: Child): string {
  return c.name?.trim() || c.preview?.trim().split("\n")[0] || c.kind;
}

/// A child's line in a node card, and the text under it. It is named by
/// its name, else who wrote it (a message reads as its author over its
/// text), else the start of its text. A name that only repeats the node
/// it is listed in (a channel's threads, each named for the channel)
/// says nothing, and the next one leads instead. The text under it is its
/// preview, less a first line the title already says.
export function rowParts(
  c: Child,
  listedIn: string | null = null,
): { title: string; snippet: string | null } {
  const name = c.name?.trim() || null;
  const named = name && name !== listedIn?.trim() ? name : null;
  const preview = c.preview?.trim() || null;
  const [first, ...rest] = preview ? preview.split("\n") : [];
  const title = named || c.author?.trim() || first?.trim() || name || c.kind;
  const snippet = preview && first?.trim() === title ? rest.join("\n").trim() || null : preview;
  return { title, snippet };
}

/// What a row of a node card is titled by, as a chip where it names
/// something: a container by its node, a message by its author, a
/// document by itself; anything else by its words.
export type RowTitle =
  | { kind: "node"; id: string; name: string; nodeKind: string; doc: string | null }
  | { kind: "person"; handle: string; shown: string }
  | { kind: "document"; page: string; title: string }
  | { kind: "text"; text: string };

export function rowTitle(c: Child, listedIn: string | null = null): RowTitle {
  const { title } = rowParts(c, listedIn);
  if (c.container && UUID.test(c.uuid)) {
    return { kind: "node", id: c.uuid, name: title, nodeKind: c.kind, doc: c.markdown_uuid };
  }
  if (c.author_handle && title === c.author?.trim()) {
    return { kind: "person", handle: c.author_handle, shown: title };
  }
  if (c.markdown_uuid && c.markdown_uuid === c.uuid && UUID.test(c.uuid)) {
    return { kind: "document", page: c.uuid, title };
  }
  return { kind: "text", text: title };
}

/// Whether a list's rows say their kind: only where it holds more than
/// one kind, since a channel's threads would each say "Slack Thread".
export function kindsDiffer(children: Child[]): boolean {
  return new Set(children.map((c) => c.kind)).size > 1;
}

/// The card a node of the tree opens.
export function nodeCardSource(node: TreeNode): string {
  return nodeSource(nodeOpts(node));
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

/// The card a child of a node card opens: a container's own children, or
/// the document a row is in, at the row when it is one of its messages.
export function childCardSource(c: Child): string | null {
  if (c.container) {
    const doc = c.markdown_uuid ? { doc: c.markdown_uuid } : {};
    return nodeSource({ node: c.uuid, name: childLabel(c), ...doc });
  }
  return previewArgs(c) && documentSource(...(previewArgs(c) as [string, string | null]));
}

/// The document a child is read in, and the section to show: a row's
/// document at the row, unless the row is that document's own.
export function previewArgs(c: Child): [string, string | null] | null {
  if (!c.markdown_uuid) return null;
  return [c.markdown_uuid, c.uuid === c.markdown_uuid ? null : c.uuid];
}

/// Whether a list of children reads best beside a preview: some of them
/// are in documents. A level of containers alone (an account's labels) is
/// a list and nothing more.
export function wantsPreview(children: Child[]): boolean {
  return children.some((c) => c.markdown_uuid !== null);
}
