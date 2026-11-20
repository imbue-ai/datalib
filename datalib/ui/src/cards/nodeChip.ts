// What a chip naming a node of a source's tree is (docs/dev/chips.md § "A
// node"): a channel, a label, a folder, a page — any `data_nodes` row.
// Like a document's, it has no resolver: whoever draws it has the node's
// name and kind from the index, which change only when its source renders
// again. The rules are pure; `nodeCell` is the one function that makes
// DOM, through the same `drawChip` every chip uses.

import { parseNode, writeNode } from "@/uri";
import { drawChip, type ChipLook } from "./contacts";
import { nodeSource } from "./cardSources";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** How a node chip looks: its name, and on hover its kind and its name
 *  as a link. */
export function nodeLook(uri: string, name: string, kind: string): ChipLook {
  return {
    text: name,
    ariaLabel: `${name}, ${kind}`,
    classes: ["handle-chip", "node-chip"],
    initial: null,
    icon: null,
    photo: null,
    title: [name, kind, uri].join("\n"),
  };
}

/** What double-clicking it opens: the node's card, its children listed
 *  and, for a node that is a document, the document beside them. */
export function nodeCardSource(uri: string, name: string, doc: string | null): string | null {
  const node = parseNode(uri);
  if (!node) return null;
  return nodeSource({ node, name, ...(doc ? { doc } : {}) });
}

export type NodeChipMenuId = "open" | "copy-id" | "copy-name" | "copy-both";
export type NodeChipEntry = { id: NodeChipMenuId; label: string; separator?: boolean };

/** The right-click menu: open it, then its copies, in the order
 *  `CHIP_ACTIONS` gives every chip's menu. No search names a node yet, so
 *  there is no "Everything in". */
export function nodeMenu(uri: string, name: string): NodeChipEntry[] {
  if (!parseNode(uri)) return [];
  return [
    { id: "open", label: `Open “${name}”` },
    { id: "copy-id", label: "Copy ID", separator: true },
    { id: "copy-name", label: `Copy “${name}”` },
    { id: "copy-both", label: "Copy name and link" },
  ];
}

/** A node's chip. `id` is its uuid, and `doc` the document it is, when it
 *  is one; anything not a uuid draws no chip, since the link goes into a
 *  card's source. */
export function nodeCell(
  id: string,
  name: string,
  kind: string,
  doc: string | null,
): HTMLAnchorElement | null {
  if (!UUID.test(id)) return null;
  const uri = writeNode(id);
  const a = document.createElement("a");
  a.className = "chip";
  a.href = uri;
  a.dataset.node = uri;
  if (doc && UUID.test(doc)) a.dataset.nodeDoc = doc;
  a.dataset.shownAs = name;
  drawChip(a, nodeLook(uri, name, kind));
  return a;
}
