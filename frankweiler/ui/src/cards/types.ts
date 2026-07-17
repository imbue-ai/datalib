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
// Structural operations — opening and closing cards — are host
// commands on the ctx, NOT bus messages. When the grid card wants a
// document card to appear next to it, it calls
// `ctx.host.openCards('documentView("abcd…")')` with the source of
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
// instance, pre-bound to that card. What "opening" means is up to the
// active layout: the miller layout opens a column to the right
// (replacing everything further right), the tree layout spawns a
// child node pointing from this card.
export type HostCommands = {
  // Open a chain of cards. The first source opens "from" this card;
  // each subsequent source opens from the card the previous source
  // produced — i.e. `openCards(a, b, c)` is `openCard(a)` from this
  // card, then `openCard(b)` from a, then `openCard(c)` from b.
  // Layout-dependent placement (see above): in the miller layout the
  // chain lays out as consecutive columns to the right (replacing
  // everything further right, so re-opening swaps the panels); in the
  // tree layout it's a parent→child spine; in the tiling layout each
  // is a sibling of the previous. Returns the new cards' ids in chain
  // order. Calling with a single source opens one card, the common
  // case (a grid row → its document).
  openCards(...sources: string[]): string[];
  // Replace THIS card's own source (and clear its state, since the old
  // state no longer applies to new code). Layout-agnostic: the miller
  // layout rewrites the column's URL segment, the tree layout rewrites
  // the node. Used by the agent hand-off to repoint a card at a freshly
  // minted component alias.
  setSource(source: string): void;
  // Close this card.
  close(): void;
  // Replace this card's persisted state string. The string is opaque
  // to the host — in the miller layout it lands in the card's URL
  // segment (`code:state`); the card decides the format. Setting ""
  // clears it (a column with empty state serializes as bare `code`).
  setState(state: string): void;
};

export type CardCtx = {
  cardId: string;
  // The card's persisted state string, as read from the URL at load
  // (or "" when absent). Opaque to the host; same string the card
  // last passed to host.setState.
  initialState: string;
  // Replace the card's human-readable title, shown in the chrome bar
  // instead of the source when dev mode is off (see devMode.ts). This
  // is the ONLY title channel: a card typically calls it first thing
  // in its render (computing the title from its arguments — e.g.
  // `gridView({ q: "kraken" })` titles itself "Search: kraken") and
  // again whenever a better title emerges — the grid retitles as the
  // user searches, the document view switches from "Document" to the
  // document's actual name once fetched. The host resets the title on
  // every (re)compile, so a card that never calls it gets the
  // source-derived fallback (title.ts displayTitle). null also means
  // "back to the fallback".
  setTitle(title: string | null): void;
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
  // Parameter-less gallery stand-in for documentView: lists every
  // rendered document (/api/docs) and, on pick, replaces this card
  // with `documentView("<uuid>")` via host.setSource.
  documentPickerView: () => CardRender;
  // The new-card gallery: parameter-less components with short
  // descriptions (builtins first, then described aliases); picking one
  // replaces this card via host.setSource. This is what non-dev card
  // creation opens (see the layout hosts).
  galleryView: () => CardRender;
  // Live listing of the user-defined component library (/api/lib).
  aliasView: () => CardRender;
  // DACTAL explorer (https://dactal.org): query grid_rows with DACTAL's
  // query language + table UI, mounted in an iframe (public/dactal/).
  // `load` is a Frankweiler search that seeds the working set; `q` is the
  // initial DACTAL query.
  dactalView: (opts?: { load?: string; q?: string }) => CardRender;
  // Scaife-like control panel over the Perseus corpus: togglable
  // versions (editions in various languages) + a book→chapter→section
  // locator tree. Clicking a locator opens one reader panel per enabled
  // version via host.openCards. See cards/libs/perseusView.ts.
  perseusView: () => CardRender;
};
