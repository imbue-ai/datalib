// Shapes for the card-based miller view.
//
// A column IS a card, and a card is defined by a piece of JS source —
// an expression like `gridView()` or `documentView("abcd…")` — that
// the host shows in the column's header and evaluates with the view
// factories in scope (see cardSource.ts). The expression must produce
// a CardRender: a function that takes a ShadowRoot and a CardCtx and
// returns a Teardown. The host (MillerView) mounts each card inside
// its own Shadow DOM and runs the render function there.
//
// Structural operations — opening and closing columns — are host
// commands on the ctx, NOT bus messages. When the grid card wants a
// document column to appear next to it, it calls
// `ctx.host.openColumn('documentView("abcd…")')` with the source of
// the new card. The bus is reserved for ambient cross-card events:
// today, the document view advertises the edge under the cursor on
// `edge.hover` so whichever card shows the destination doc can
// highlight the target span.

export type Teardown = () => void;

export type BusMeta = { from?: string };
export type BusHandler = (payload: unknown, meta: BusMeta) => void;

export type Bus = {
  publish(topic: string, payload: unknown, opts?: { from?: string }): void;
  subscribe(topic: string, handler: BusHandler): Teardown;
};

// Commands a card can issue against the host. Each card gets its own
// instance, pre-bound to that card's column.
export type HostCommands = {
  // Open a new column directly to the right of this card, replacing
  // any columns currently to its right (Miller semantics). `source` is
  // the card source of the new column, e.g. `documentView("abcd…")`.
  // Returns the new card's id.
  openColumn(source: string): string;
  // Close this card's column.
  close(): void;
  // Replace this card's persisted state string. The string is opaque
  // to the host — it just lands in the column's URL segment
  // (`code:state`); the card decides the format. Setting "" clears it
  // (a column with empty state serializes as bare `code`).
  setState(state: string): void;
};

export type CardCtx = {
  cardId: string;
  // The card's persisted state string, as read from the URL at load
  // (or "" when absent). Opaque to the host; same string the card
  // last passed to host.setState.
  initialState: string;
  bus: Bus;
  host: HostCommands;
};

export type CardRender = (root: ShadowRoot, ctx: CardCtx) => Teardown;

// Bus topic: the destination of the edge currently under the cursor.
// Published by the document view when the pointer enters an
// edge-source span (or a doc-level outgoing-edge link); published
// with a null payload when the pointer leaves. Subscribing document
// views match `markdownUuid` against their own doc and put a
// transient highlight on the `sectionUuid` span.
export const TOPIC_EDGE_HOVER = "edge.hover";

export type EdgeHoverPayload = {
  markdownUuid: string;
  // Anchor inside the destination doc; null when the edge points at
  // the whole document (no span highlights in that case).
  sectionUuid: string | null;
} | null;

// A view factory takes view-specific arguments and returns a
// CardRender. These are the names in scope when card source is
// evaluated; `gridView()` in a card's source calls ViewLibs.gridView.
export type ViewLibs = {
  gridView: (opts?: { q?: string }) => CardRender;
  documentView: (
    markdownUuid?: string | null,
    sectionUuid?: string | null,
  ) => CardRender;
};
