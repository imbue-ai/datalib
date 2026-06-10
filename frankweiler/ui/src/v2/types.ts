// Shapes for the v2 "everything is a card" prototype.
//
// A column IS a card, and a card is defined by a piece of JS source —
// an expression like `gridView()` or `documentView("abcd…")` — that
// the host shows in the column's header and evaluates with the view
// factories in scope (see cardSource.ts). The expression must produce
// a CardRender: a function that takes a ShadowRoot and a CardCtx and
// returns a Teardown. The host (MillerViewV2) mounts each card inside
// its own Shadow DOM and runs the render function there.
//
// Structural operations — opening and closing columns — are host
// commands on the ctx, NOT bus messages. When the grid card wants a
// document column to appear next to it, it calls
// `ctx.host.openColumn('documentView("abcd…")')` with the source of
// the new card. The bus is reserved for ambient cross-card events and
// is unused by the prebuilt views today.

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
};

export type CardCtx = {
  cardId: string;
  bus: Bus;
  host: HostCommands;
};

export type CardRender = (root: ShadowRoot, ctx: CardCtx) => Teardown;

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
