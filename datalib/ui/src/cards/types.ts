// Shapes for cards and the host that lays them out.
//
// A card is defined by a piece of JS source — an expression like
// `gridView()` or `documentView("abcd…")` — that the host shows in
// the card's header in edit mode and evaluates with the view factories
// in scope (see cardSource.ts). The expression must produce a
// CardRender: a function that takes a ShadowRoot and a CardCtx and
// returns a Teardown. The host (ContainersView) mounts each card
// inside its own Shadow DOM and runs the render function there.

export type Teardown = () => void;

export type BusMeta = { from?: string };
export type BusHandler = (payload: unknown, meta: BusMeta) => void;

export type Bus = {
  publish(topic: string, payload: unknown, opts?: { from?: string }): void;
  subscribe(topic: string, handler: BusHandler): Teardown;
};

// Commands a card can issue against the host. Each card gets its own
// instance, pre-bound to that card. Where an opened card goes is the
// host's call (docs/dev/cards.md § "The containers layout"): the
// nearest container above this card that is not solidified, placed by
// that container's layout.
export type HostCommands = {
  // Open a chain of cards. The first source opens "from" this card;
  // each subsequent source opens from the card the previous source
  // produced — i.e. `openCards(a, b, c)` is `openCard(a)` from this
  // card, then `openCard(b)` from a, then `openCard(c)` from b.
  // The whole chain lands in one container: in Columns as consecutive
  // columns to the right of this card's (replacing what was further
  // right, so re-opening swaps the panels), in Tabs as tabs each under
  // the one before. Returns the new cards' ids in chain order. Calling
  // with a single source opens one card, the common case (a grid row →
  // its document).
  openCards(...sources: string[]): string[];
  // The URL `openCards(...sources)` would land on, so a card can draw a
  // real link: a plain click goes through openCards, and a modified
  // click, a middle click, the context menu and a drag are the
  // browser's — a new tab, a copied link, a bookmark. The link is the
  // chain alone (chainHref.ts), which a new window opens as a tab.
  hrefFor(...sources: string[]): string;
  // Replace THIS card's own source (and clear its state, since the old
  // state no longer applies to new code). Used by the agent hand-off to
  // repoint a card at a freshly minted component alias.
  setSource(source: string): void;
  // Replace THIS card with a fresh copy of the composite `name` — a
  // container of cards kept under a name, such as the Dashboard
  // (views/composites.ts). The new-card gallery's composite entries.
  becomeComposite(name: string): void;
  // Close this card.
  close(): void;
  // Replace this card's persisted state string. The string is opaque
  // to the host, which keeps it with the card in the layout it saves to
  // the library; the card decides the format. Setting "" clears it.
  setState(state: string): void;
};

export type CardCtx = {
  // A UUIDv7 minted when the card opened (cardId.ts), kept for the
  // card's life and across reloads of its window.
  cardId: string;
  // What the card's current source calls (cardId.ts cardType); follows
  // host.setSource.
  readonly cardType: string;
  // The card's persisted state string, as read from the URL at load
  // (or "" when absent). Opaque to the host; same string the card
  // last passed to host.setState.
  initialState: string;
  // Replace the card's human-readable title, shown in the chrome bar
  // instead of the source when edit mode is off (see editMode.ts). This
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
  // Offer help: what this card shows and how to work it, as HTML. The
  // chrome grows a "?" that opens it. Every card should say something
  // here — a card with no help is a card that assumes its reader
  // already knows it. null takes the offer back; the host clears it
  // when the card is torn down.
  setHelp(html: string | null): void;
  bus: Bus;
  host: HostCommands;
};

export type CardRender = (root: ShadowRoot, ctx: CardCtx) => Teardown;

// Bus topic: config.toml was just written by a card on this page (the
// wizard, a rename, a delete). A card showing the file reloads on it
// rather than waiting for the data root's own change frame, which
// arrives a beat later. Payload null.
export const TOPIC_CONFIG_WRITTEN = "config.written";

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
  // The Dashboard's sections, each a card of its own; the Dashboard
  // composite lays them out as a Page (libs/dashboardSections.ts).
  syncStatusView: () => CardRender;
  needsYouView: () => CardRender;
  libraryView: () => CardRender;
  sourcesOverviewView: () => CardRender;
  latestActivityView: () => CardRender;
  // The Search card: results as a list, the picked one read in place.
  // The same search as gridView, which shows it as a table.
  searchView: (opts?: { q?: string }) => CardRender;
  gridView: (opts?: { q?: string; columns?: string[]; name?: string }) => CardRender;
  documentView: (markdownUuid?: string | null, sectionUuid?: string | null) => CardRender;
  // Parameter-less gallery stand-in for documentView: lists every
  // rendered document (the unified_index applet's /docs) and, on
  // pick, replaces this card
  // with `documentView("<uuid>")` via host.setSource.
  documentPickerView: () => CardRender;
  // The new-card gallery: parameter-less components with short
  // descriptions (builtins first, then described aliases, then the
  // create-with-an-agent entry); picking one replaces this card via
  // host.setSource. This is what every layout's "add card" opens.
  galleryView: () => CardRender;
  // The in-card hand-off instructions a freshly minted, agent-bound
  // component is seeded with: `() => agentSeedView("<name>")` (see
  // handoff.ts). The agent's first save of the alias replaces it.
  agentSeedView: (name: string) => CardRender;
  // Live listing of the user-defined component library (/api/lib).
  aliasView: () => CardRender;
  // DACTAL explorer (https://dactal.org): query grid_rows with DACTAL's
  // query language + table UI, mounted in an iframe (public/dactal/).
  // `load` is a Datalib search that seeds the working set; `q` is the
  // initial DACTAL query.
  dactalView: (opts?: { load?: string; q?: string }) => CardRender;
  // Scaife-like control panel over the Perseus corpus: togglable
  // versions (editions in various languages) + a book→chapter→section
  // locator tree. Clicking a locator opens one reader panel per enabled
  // version via host.openCards. See cards/libs/perseusView.ts.
  perseusView: () => CardRender;
  // Visualize the sync pipeline's step DAG, with live run states.
  sourceDagView: () => CardRender;
  // The typed table viewer over any endpoint that declares its columns
  // (`{columns, rows}`), e.g. `tableView({ url: "/api/manage/rows" })`.
  // See cards/TableGrid.ce.vue for the column-type vocabulary.
  tableView: (opts: { url: string }) => CardRender;
  // The Manage screen as a card: every source and step config.toml
  // declares, with status, actions and the panels they open.
  sourcesView: (opts?: { add?: boolean }) => CardRender;
  // config.toml itself, edited directly.
  configView: () => CardRender;
  // The run log: one process's lines — a step's newest attempt, the
  // runner, a launch of the server — or a whole run's, with pickers
  // to move between them. Selecting a line opens `logLineView` beside
  // it. See cards/LogCard.ce.vue.
  logView: (opts?: {
    run?: string | null;
    step?: string | null;
    launch?: string | null;
    q?: string;
    jumpToEnd?: boolean;
  }) => CardRender;
  // One log line in full — its message, fields, source and process —
  // by its store sequence number. See cards/LogLineCard.ce.vue.
  logLineView: (seq: number) => CardRender;
  // The commit history of the doltlite stores under some trees, as a
  // tree of store, commit and table; on a source, where two of its
  // versions are compared. See cards/HistoryCard.ce.vue.
  historyView: (opts: {
    trees: string[];
    title: string;
    source?: string | null;
    compare?: boolean;
  }) => CardRender;
  // Every embedded document as a point, placed by the `embedding_map`
  // step so like sits near like; filter with the grid's grammar, colour
  // by a field, hover to preview, click to open. See cards/UmapCard.ce.vue.
  umapView: (opts?: { q?: string; by?: string }) => CardRender;
  // One group's sync as a dashboard: its row and each step's, laid out
  // vertically with their actions, charts over the run and the group's
  // log. See cards/SyncDashboardCard.ce.vue.
  syncDashboardView: (opts: { group: string; step?: string }) => CardRender;
};
