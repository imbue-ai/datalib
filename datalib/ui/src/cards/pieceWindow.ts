// The window of a document's pieces the document card holds: a page of
// its top-level nodes, grown a page at a time at either end as the reader
// scrolls (docs/dev/plans/completed/document_viewer_from_blocks.md §"Step 6"). Pure:
// the card fetches, and the chat body draws what changed.
import type { BodyNode, ChatResponse } from "@/api";
import { isFold } from "./renderDocument";

/// Top-level nodes asked for at a time. A document with no more comes
/// whole, so find-in-page and select-all see all of it.
export const PAGE = 100;

/// How far from an end of what is drawn, in CSS pixels, the next page is
/// asked for.
export const NEAR_END_PX = 600;

/// How `next` follows what is drawn: the same nodes; the same with pages
/// added above or below, which are drawn beside what is there so an open
/// fold or an expanded message keeps its state; or something else, drawn
/// anew.
export type Growth =
  { kind: "same" } | { kind: "grew"; above: BodyNode[]; below: BodyNode[] } | { kind: "new" };

export function growth(drawn: readonly BodyNode[], next: readonly BodyNode[]): Growth {
  if (drawn.length === 0) return { kind: "new" };
  const at = next.indexOf(drawn[0]!);
  if (at < 0 || at + drawn.length > next.length) return { kind: "new" };
  if (drawn.some((n, i) => next[at + i] !== n)) return { kind: "new" };
  if (at === 0 && next.length === drawn.length) return { kind: "same" };
  return {
    kind: "grew",
    above: next.slice(0, at),
    below: next.slice(at + drawn.length),
  };
}

/// The window with a page added above or below it, the window's nodes
/// kept as they are so the chat body sees them grow.
export function withPage(
  win: ChatResponse,
  page: ChatResponse,
  where: "above" | "below",
): ChatResponse {
  return where === "above"
    ? { ...win, pieces: [...page.pieces, ...win.pieces], before: page.before ?? null }
    : { ...win, pieces: [...win.pieces, ...page.pieces], after: page.after ?? null };
}

/// Whether `nodes` hold the piece of item `uuid`, inside a fold or not.
export function holds(nodes: readonly BodyNode[], uuid: string): boolean {
  return nodes.some((n) => (isFold(n) ? holds(n.pieces, uuid) : n.uuid === uuid));
}
