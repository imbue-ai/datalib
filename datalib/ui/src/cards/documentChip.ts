// What a chip naming a rendered document is (docs/dev/chips.md § "A
// document"): how it looks, what its double-click opens and its menu. A
// document's chip has no resolver: its producer sends the title, which
// changes only when the document is rendered again, and the grid row the
// producer read changes with it. The rules are pure; `documentCell` is
// the one function that makes DOM, through the same `drawChip` every
// chip uses.

import { parseDoc, writeDoc } from "@/uri";
import { drawChip, type ChipLook } from "./contacts";
import { documentSource } from "./cardSources";

/** How a document chip looks: its title, led by its source's mark. */
export function documentLook(
  uri: string,
  title: string,
  icon: string | null,
  sourceName: string | null,
): ChipLook {
  return {
    text: title,
    ariaLabel: sourceName ? `${title}, a document from ${sourceName}` : `${title}, a document`,
    classes: ["handle-chip", "doc-chip"],
    initial: null,
    icon,
    photo: null,
    title: documentTooltip(uri, title, sourceName),
  };
}

/** The hover: the title, where it came from, and its name. */
export function documentTooltip(uri: string, title: string, sourceName: string | null): string {
  return [title, ...(sourceName ? [`From ${sourceName}`] : []), uri].join("\n");
}

/** `Title (datalib:doc/<uuid>)`, as every chip copies itself. */
export function documentCopyText(uri: string, title: string): string {
  return `${title} (${uri})`;
}

/** The same as a link, for whatever pastes rich text: the title, linked
 *  to the document's name. */
export function documentCopyHtml(uri: string, title: string): string {
  const escape = (v: string) => v.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
  return `<a href="${escape(uri)}">${escape(title)}</a>`;
}

/** What double-clicking it opens: the document's card, at its section
 *  when the URI names one. */
export function documentCardSource(uri: string): string | null {
  const name = parseDoc(uri);
  return name ? documentSource(name.page, name.section) : null;
}

/** The document's uuid: what Copy ID copies. */
export function documentId(uri: string): string | null {
  return parseDoc(uri)?.page ?? null;
}

export type DocumentChipMenuId = "open" | "copy-id" | "copy-name" | "copy-both";
export type DocumentChipEntry = { id: DocumentChipMenuId; label: string; separator?: boolean };

/** The right-click menu: open it, then its copies, in the order
 *  `CHIP_ACTIONS` gives every chip's menu. The rest of the document title
 *  bar's menu (open at the source, feedback) needs the document itself,
 *  which a chip does not hold. */
export function documentMenu(uri: string, title: string): DocumentChipEntry[] {
  if (!parseDoc(uri)) return [];
  return [
    { id: "open", label: `Open “${title}”` },
    { id: "copy-id", label: "Copy ID", separator: true },
    { id: "copy-name", label: `Copy “${title}”` },
    { id: "copy-both", label: "Copy title and link" },
  ];
}

/** A document chip for a grid cell. `uri` is a `datalib:doc/` name,
 *  written again so a chip never carries a link the reader would not
 *  write itself. */
export function documentCell(
  uri: string,
  title: string,
  icon: string | null,
  sourceName: string | null,
): HTMLAnchorElement | null {
  const name = parseDoc(uri);
  if (!name) return null;
  const doc = writeDoc(name);
  const a = document.createElement("a");
  a.className = "chip";
  a.href = doc;
  a.dataset.doc = doc;
  a.dataset.shownAs = title;
  drawChip(a, documentLook(doc, title, icon, sourceName));
  return a;
}
