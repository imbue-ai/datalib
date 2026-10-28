// What a rendered document offers wherever it is drawn whole: the
// buttons and the ⋯ menu of its title bar (DocTitleBar.ce.vue), and the
// menu a document chip will show once there is one (docs/dev/chips.md
// § "Not built"). Pure; the card showing the document does what an
// entry names (DocCard.ce.vue).
import type { ChatResponse } from "@/api";
import { filePathFromUrl } from "@/desktop";
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
  | "copy-id"
  | "copy-name"
  | "copy-both"
  | "copy-markdown"
  | "feedback";

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

/** `Title (datalib:doc/<uuid>)`, as a group chip copies itself. */
export function documentCopyText(doc: DocumentAbout): string {
  return `${documentTitle(doc)} (${writeDoc({ page: doc.markdown_uuid, section: null })})`;
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
    { id: "feedback", label: "Feedback on this document…" },
  );
  // A line before the copies and before Feedback, when something is above it.
  const firstCopy = out.findIndex((e) => e.id === "copy-id");
  if (firstCopy > 0) out[firstCopy] = { ...out[firstCopy], separator: true };
  out[out.length - 1] = { ...out[out.length - 1], separator: true };
  return out;
}
