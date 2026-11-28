// What a rendered document offers wherever it is drawn whole: the
// buttons and the ⋯ menu of its title bar (DocTitleBar.ce.vue). A
// document's chip offers the part of it a chip can do
// (documentChip.ts). Pure; the card showing the document does what an
// entry names (DocCard.ce.vue).
import type { ChatResponse } from "@/api";
import { filePathFromUrl } from "@/desktop";
import { outsideLink } from "@/outsideLink";
import { writeDoc } from "@/uri";

export type DocumentAbout = Pick<
  ChatResponse,
  "markdown_uuid" | "name" | "source_label" | "source_url" | "source_ref"
>;

export type DocumentAction =
  | "open-card"
  | "open-url"
  | "reveal"
  | "copy-path"
  | "open-source"
  | "copy-link"
  | "copy-id"
  | "copy-name"
  | "copy-both"
  | "copy-markdown";

export type DocumentEntry = { id: DocumentAction; label: string; separator?: boolean };

/** Where a document came from, as the one entry that goes there: a web
 *  page opens, a local file is revealed where the app can and its path
 *  copied where it cannot. */
export type Outlink = { id: "open-url" | "reveal" | "copy-path"; label: string; target: string };

export type Host = {
  /** Shown beside a list rather than as a card of its own, so it can
   *  be opened as one. */
  preview: boolean;
  /** The desktop app's name for revealing a file; null in a browser,
   *  which cannot. */
  revealLabel: string | null;
};

export function documentTitle(doc: DocumentAbout): string {
  return doc.name?.trim() || "Untitled document";
}

/** `datalib:doc/<Title>-<uuid>`, what the title bar's 🔗 copies. */
export function documentLink(doc: DocumentAbout): string {
  return writeDoc({ page: doc.markdown_uuid, section: null }, doc.name ?? "");
}

/** `Title (datalib:doc/<uuid>)`, as a group chip copies itself. */
export function documentCopyText(doc: DocumentAbout): string {
  const name = writeDoc({ page: doc.markdown_uuid, section: null });
  return `${documentTitle(doc)} (${outsideLink(name)})`;
}

export function outlinkOf(doc: DocumentAbout, host: Host): Outlink | null {
  const url = doc.source_url;
  if (!url) return null;
  const path = filePathFromUrl(url);
  if (path === null) {
    const label = doc.source_label ? `Open in ${doc.source_label}` : "Open at the source";
    return { id: "open-url", label, target: url };
  }
  return host.revealLabel
    ? { id: "reveal", label: host.revealLabel, target: path }
    : { id: "copy-path", label: "Copy file path", target: path };
}

export function documentMenu(doc: DocumentAbout, host: Host): DocumentEntry[] {
  const out: DocumentEntry[] = [];
  if (host.preview) out.push({ id: "open-card", label: "Open as card" });
  const link = outlinkOf(doc, host);
  if (link) out.push({ id: link.id, label: link.label });
  if (doc.source_ref)
    out.push({ id: "open-source", label: `Open the ${doc.source_ref.label} source` });
  out.push(
    { id: "copy-id", label: "Copy ID" },
    { id: "copy-name", label: `Copy “${documentTitle(doc)}”` },
    { id: "copy-both", label: "Copy title and link" },
    { id: "copy-markdown", label: "Copy as Markdown" },
  );
  // A line before the copies, when something is above them.
  const firstCopy = out.findIndex((e) => e.id === "copy-id");
  if (firstCopy > 0) out[firstCopy] = { ...out[firstCopy], separator: true };
  return out;
}
