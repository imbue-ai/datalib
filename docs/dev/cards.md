# Cards — source-defined cards and the layouts that host them

The datalib UI is a surface of cards: each card is a piece of
JavaScript source the user can read (and edit) in the card's header
bar in edit mode. The host evaluates that source to produce the card's content.
`datalib/ui/src/cards/types.ts` is the canonical home of every
shape described here; this doc is the narrative version.

The **containers layout** arranges cards on screen
(`datalib/ui/src/views/ContainersView.vue`; § "The containers layout"
below). Most of this doc is the contract between a card and that host:
what a card gets, what it can ask for, and what it must not assume.

## Card source

A card's source is a JS **expression**, e.g.

```js
gridView()
documentView("e28ed67d-507b-5319-8732-00e249b6ebf6")
documentView("e28ed67d-…", "11ec65e9-…")   // doc + section to highlight
```

`compileCardSource` (`datalib/ui/src/cards/cardSource.ts`) wraps
the expression in `new Function(...viewLibNames, "return (<source>)")`
and calls it with the view factories as arguments, plus one more name:
`comp`. Two kinds of name are in scope, then, along with JS globals —
the builtin factories in `ViewLibs`
(`datalib/ui/src/cards/libs/index.ts`), and every custom component as
`comp.<namespace>.<name>`.

`comp` is namespaced because the components come from a store that is
too: `system/frontend/user/` holds what a person or an agent wrote, and
one directory per applet holds what that applet wrote (see
`docs/dev/applets.md`). A component name is a member of its namespace,
never a global — which is what lets two applet instances both offer
`channels`, and what lets a user component be called `gridView` without
shadowing the builtin.

Because the source is plain JS, a user-authored card is just a bigger
expression. An IIFE that composes the factories works:

```js
(() => {
  // decide arguments programmatically, then delegate
  return documentView("e28ed67d-…");
})()
```

## CardRender: the rendering contract

```ts
type CardRender = (root: ShadowRoot, ctx: CardCtx) => Teardown;
type Teardown = () => void;
```

Whatever the layout, the host mounts each card inside its own **shadow
root** (via `datalib/ui/src/components/ShadowCard.vue`) and calls
the render function with it. The card owns that DOM completely — the
host renders nothing inside. The returned teardown runs when the card
closes or its source is re-run after an edit.

Shadow DOM is the isolation boundary: document-head styles do not
reach inside, so a card must inject any CSS it needs into `root`
itself. CSS custom properties (the app's `--datalib-*` theme variables) do
inherit across the boundary and are the supported way to pick up
theming. They live in `datalib/ui/src/theme.css`: the colours for
light and dark, the system font, and every size — row height, control
height, padding, font sizes. The spacing ones — row and control
heights, padding, gaps — are drawn from one number,
`--datalib-density`, which the status bar's **density** sets on `<html>`
(`datalib/ui/src/density.ts`; the scale is `densityScale.ts`: 0 to 1 in
eighths). Text keeps one size; the browser's zoom (⌘+ / ⌘−,
turned on in the desktop app too) makes everything larger. A card that
sizes itself with `var(--datalib-row-h)`, `var(--datalib-pad)` and the
rest follows the density without knowing it exists. `--datalib-bg` is a
card's own background; `--datalib-ground` is the grey the cards sit
on.

The prebuilt cards are Vue components, adapted to this contract by
`vueCard` (`datalib/ui/src/cards/vueCard.ts`): it injects the
component's compiled styles into the shadow root, mounts a dedicated
Vue app with the ctx as a prop, and returns `app.unmount` as the
teardown. Card components use the `*.ce.vue` suffix so
`@vitejs/plugin-vue` compiles them in custom-element mode, which
attaches their `<style>` blocks as `component.styles` (strings)
instead of injecting them into the document head. Child components of
a card must also be `.ce.vue` and listed in the adapter's
`styleSources` so their CSS lands in the root too (see
`datalib/ui/src/cards/libs/documentView.ts` for the pattern).

## Titles, icons and edit mode

Every card kind describes itself with the same three fields: a
**title** and **description** for the gallery, and an **icon** the
layouts draw beside the card's name — on a tab, in a card's header, in
the gallery. A builtin declares them in `datalib/ui/src/cards/catalog.ts`
(`BUILTIN_META`, keyed by `ViewLibs`, so a new builtin without an entry
does not type-check); a custom component declares them in its
`<name>.json` (see [`applets.md`](applets.md)). `cardMeta(source)`
answers for either, and `components/CardIcon.vue` draws what it names,
so nothing that draws a card knows which kind it is. An icon is a
glyph name from `cards/icons.ts`, a source's mark in `src/assets/`, or
a `data:image/…` URL; anything else draws the generic component glyph.

The header around each card has two faces, switched by the **Edit**
toggle in the status bar (`datalib/ui/src/editMode.ts`, persisted in
localStorage):

- **Edit mode off** (the default): the bar shows the card's
  human-readable **title** — read-only, no code visible.
- **Edit mode on**: the bar shows the card's source in the editable box
  described below (Enter re-runs the card).

The 🤖 agent hand-off button shows in **both** modes, but only on cards
backed by a user-defined component (the source's callee is an alias in
the `/api/lib` manifest — builtins live in the app bundle, so there's
nothing an agent could modify). Pressing it walks the user through
handing the component to a coding agent (`datalib/ui/src/handoff.ts`
and `components/AgentHandoffModal.vue`): a step list whose first step
copies a "wayfinder" prompt, plus a persisted "skip these steps next
time" opt-out that turns the button into a straight copy.

Every way to add a card — the ＋ strip at the end of a Columns
container, the sidebar's "＋ New card", a container panel's Add → Card —
creates a `galleryView()` card — the **new-card gallery**
(`datalib/ui/src/cards/libs/galleryView.ts`). It lists, each with a
short description:

1. the composites (`views/composites.ts`), the Dashboard first; picking
   one replaces the gallery card with a copy of it
   (`host.becomeComposite`);
2. the builtins `cards/catalog.ts` offers, `sourcesView` first. A
   builtin marked `galleryHidden` — a building block such as a
   Dashboard section — is listed only once the gallery's "Show every
   view" switch is on (kept in this browser);
3. every titled component in the frontend store, each expanding to its
   qualified name called with its stored `component_args` — so one
   component appears once per namespace with its own arguments
   (`comp.slack_work.channels("slack_work")`,
   `comp.slack_personal.channels(…)`);
4. a "build a component with an agent" entry that mints a fresh
   component seeded with `agentSeedView` (the in-card hand-off
   instructions) and repoints the card at it.

A builtin in the gallery takes no arguments, so one that needs them
offers a parameter-less stand-in: `documentView`'s is
`documentPickerView()`. In edit mode the gallery also shows each entry's
source and a note that source can be typed straight into the chrome
bar. Picking an entry replaces the gallery card with the chosen
component via `host.setSource`. An agent can rename a component
(`POST /api/lib/{name}/rename`); the store leaves a tombstone and
`ShadowCard` rewrites any card still referencing the old name.

A card sets its title with `ctx.setTitle`, usually first thing in its
render — and again whenever a better title emerges, so titles are
live, not fixed at factory-call time:

```ts
export function galleryView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("New card");
    // …
  };
}
```

The grid card retitles itself (`Search: <q>`) as the user searches,
unless its source gives it a name (`gridView({ name: "Slack documents" })`,
which is what Browse opens);
the document card starts as "Document" and switches to the document's
actual name once its fetch lands. This works the same for builtin
factories and user-defined aliases. The host resets the title on every
(re)compile, so a card that never calls `setTitle` — and the blank /
error states — gets a best-effort fallback (`displayTitle`,
`datalib/ui/src/cards/title.ts`): the bare factory/alias name for
`name(...)`-shaped source, `new card` for a blank card, or a generic
label for anything else.

## CardCtx: what a card receives

```ts
type CardCtx = {
  cardId: string;        // a UUIDv7 minted when the card opens (cards/cardId.ts)
  cardType: string;      // what its source calls: `gridView`, `comp.user.tetris`
  initialState: string;  // persisted state from the host ("" when absent)
  setTitle(title: string | null): void;  // chrome-bar title (see above)
  setHelp(html: string | null): void;    // the chrome's "?" (see below)
  bus: Bus;              // ambient cross-card events
  host: HostCommands;    // structural + persistence commands
};
```

A card's requests say which card made them: a component inside a card
takes the api from `useApi()` (`cards/cardApi.ts`), a plain-DOM card
from `cardApi(ctx)`, and every request they make carries
`X-Datalib-Card` and `X-Datalib-Card-Type`, which the server's request
log records. The scope ends at the first `await`, so a function in
`api.ts` calls `fetch` before awaiting anything
(`tests/card_api.test.ts` checks).

### Help

Every card should offer help: what it shows and how to work it, as
HTML, through `ctx.setHelp` — usually right after `setTitle`. The chrome
grows a "?" that opens it in a popup over the page (`CardControls.vue`;
the text is kept per card id in `cards/help.ts`, so every layout
shares one mechanism). A card with no help is a card that assumes its
reader already knows it. The host clears the offer when the card is
torn down, as it does the title.

### HostCommands

Each card gets its own instance, pre-bound to that card:

```ts
type HostCommands = {
  openCards(...sources: string[]): string[];  // returns the new cards' ids
  hrefFor(...sources: string[]): string;      // the URL openCards would land on
  setSource(source: string): void;
  close(): void;
  setState(state: string): void;
};
```

- `openCards(...sources)` opens a chain of new cards "from" this one
  (one source is the common case). The card supplies only the new
  cards' **source** — e.g. the grid card composes
  `documentView("<md>", "<row>")` when a row is clicked — and makes
  **no assumption about placement**: where the new card lands is the
  host's business (§ "The containers layout"). Structural operations
  always go through host commands, never the bus.
- `hrefFor(...sources)` is the URL `openCards(...sources)` would land
  on, so a card can draw a **real link**: a plain click goes through
  `openCards`, and a modified click, a middle click, the context menu
  and a drag are the browser's — a new tab, a copied link, a bookmark.
  The document card's edge list is the pattern (`onEdgeLinkClick`;
  `isBrowserClick` in `cards/chatLink.ts` is the one rule for which
  clicks to leave alone). The link names the chain alone, which a new
  window opens as a tab.
- `close()` closes this card. The host may close more with it (a tab
  takes the tabs opened from it) — that's its call, not the card's.
- `setState(state)` replaces this card's persisted state string (see
  below).

### State strings

A card may persist state so it survives a re-run and a reload. The
string is **opaque to the host**: the card passes whatever it likes to
`setState`, the host keeps it with the card in the layout it saves, and
the card reads it back as `ctx.initialState`. Setting `""` clears it. A
saved layout can be lost or unreadable, so a card must treat
`initialState` as a best-effort restore, never a guarantee.

The grid card is the reference user
(`datalib/ui/src/cards/GridCard.ce.vue`): it keeps
`URLSearchParams` of `q` (search query), `sel` (selected row uuid) and
`cols` (the grid's layout — column order, visibility and widths, the
sort, the grouping — as base64url-encoded JSON), writing only on
user-driven changes so a pristine grid keeps clean state.

### Bus

```ts
type Bus = {
  publish(topic: string, payload: unknown, opts?: { from?: string }): void;
  subscribe(topic: string, handler: BusHandler): Teardown;  // returns unsubscribe
};
```

The bus is for **ambient cross-card events** — things any number of
cards may care about, where the publisher doesn't know (or pick) the
receiver. It carries no structural operations. The topics:

- `edge.hover` (`TOPIC_EDGE_HOVER`): a document card publishes the
  destination of the edge under the cursor
  (`{ markdownUuid, sectionUuid } | null`), and every document card
  subscribes, matching `markdownUuid` against its own doc to put a
  transient highlight on the target span.
- `config.written` (`TOPIC_CONFIG_WRITTEN`): a card just wrote
  `config.toml`; `configView` reloads.
- `log.query`: a log line card's *keep* / *exclude* buttons send a
  token to the log card's query bar.

Payloads cross card boundaries as `unknown`; subscribers validate the
shape before acting. Unsubscribe in the card's teardown.

## The containers layout

`ContainersView.vue` hosts every card; the rules are pure functions in
`containerTree.ts`, tested in `tests/container_tree.test.ts`. The
layout is a tree of **containers** whose leaves are cards, and each
container lays out its own children:

- **Tabs** shows one child at a time. The outermost container is always
  Tabs, drawn as the sidebar: each tab under the tab it was opened
  from, as in Firefox's Tree Style Tab.
- **Page** puts children one after another at their natural height and
  scrolls. A card there is as tall as its content (`ShadowCard`'s
  `natural`, honoured by `vueCard`).
- **Split** shares the space side by side or top to bottom (its
  direction), with dividers to resize.
- **Columns** puts children side by side at set widths and scrolls
  sideways; a column that appears is scrolled into view.

**Where an opened card goes.** A container can be **solidified**, which
holds for everything inside it too. When a card opens cards
(`openCards`), they land in the nearest container above the opener that
is not solidified, and that container's layout decides where: Columns
drop what was right of the opener's column and add the chain; a Split or
Page insert it after the opener's child; Tabs give each card a tab of
its own, filling it, under the tab of the card before. The outermost
container is never solidified, so an open always lands somewhere. A
card opened from the toolbar (Logs, Data sources, a search) is a tab of
its own. A link opens its cards as one tab holding a Columns container,
so what they open lands beside them.

**What shows.** Outside edit mode a solidified subtree shows no card
chrome, so a composite such as the Dashboard (a Page of its five
section cards) reads as one page; elsewhere each card
has a header with its title and controls, and a Columns container ends
in a ＋ strip that adds a card. In edit mode every card shows its edge
and its source, and each container is a frame coloured by its layout,
with a folder tab on its top edge that opens its panel (layout,
Solidified, add a card or container, move, put in a new container, take
the cards out, rename, save as composite, reset, close). The frame's
edge is dashed while cards open into it, and a thick solid line once
it is solidified.

**Names.** A tab or container is named by its first card until the
person renames it (its panel, or a double-click on a tab's name); a
card's own `ctx.setTitle` never replaces a name the person gave. The
page's title names the tab shown.

**Composites.** A composite is a container subtree kept under a name
(`composites.ts`): the built-in Dashboard, or one a person saved. A
container made from one can be reset to it.

**Keeping it.** The main window — the first open (`mainWindow.ts`) —
keeps the tree, and the saved composites, in the library through
`/api/ui/state/{name}` (`docs/dev/app_stores.md`), not the browser: the
desktop app's server takes a new port, and so a new origin, each
launch. Any other window (a card popped out with ↗) keeps its own tree
in its session storage, so a reload finds its tabs. A browser with
`datalib-layout-unsaved` set in localStorage does the same in its main
window; the e2e suite sets it, so specs sharing one library do not
trade tabs.

**The URL.** A path naming cards (`/code:size:state/…`,
`router/columns.ts`) is a link: the layout opens those cards as a tab
and puts the address back to `/`, since what is open is kept in the
tree. A `/chat/<uuid>` link — the shape every renderer writes into a
document body — is routed to that document alone (`router/index.ts`).
The browser's Back and Forward do not step through cards.

## How a card and its layout interact

Everything a card needs from its surroundings is the `CardCtx`; it
never reaches for the layout directly. The division of labour:

- **The card** owns its shadow root, renders into it, persists its own
  opaque state, and asks for new cards / closure through host commands.
- **The layout** owns placement and chrome. Around each card it draws
  a header with the card's title (in edit mode, the source box: Enter
  re-runs the card, Shift+Enter inserts a newline; committing new
  source clears the old state string), an ↗ "open this card alone"
  link — a new browser tab, or in the desktop app a second native
  window of the app (the shell's `on_new_window` handler in
  `datalib/tauri/src/main.rs`), so a card can live on another screen —
  and a ✕ close button. Anything past that — resize handles, frames,
  add strips, tab strips — is layout furniture, invisible to the card.
  The layout also decides what `openCards` placement means and what
  `close` takes with it.

**A card, not a modal**, for anything a person reads, keeps open or
clicks through from — a log, a commit history, a table. It sits in the
layout beside what opened it and is kept with the layout. A modal
dialog is for a question that has to be answered before anything else
happens: a confirm, a short form that is submitted or cancelled (the
source wizard, feedback).

## Prebuilt views

The factories in `ViewLibs` are the public surface card source
programs against:

- The Dashboard's sections, each a card of its own, which the Dashboard
  composite — what a new window opens on — lays out as a solidified
  Page (`cards/libs/dashboardSections.ts`; hidden from the gallery
  until it shows every view):
  - `syncStatusView()` — when the library last synced, and Sync now /
    Stop syncing; "Start your first sync" when its sources never have;
  - `needsYouView()` — a source whose last sync failed or stopped, or
    whose store holds errors or warnings, with the fix beside it;
    nothing at all when nothing needs the person;
  - `libraryView()` — the library's item count, its size on disk, and
    a bar of what takes the space;
  - `sourcesOverviewView()` — each source's state, when it synced, its
    items and size; "No sources yet." and "Add source" in a library
    without any;
  - `latestActivityView()` — the newest documents by their own
    timestamps.

  Each section is a component (`cards/Dashboard*.ce.vue`) drawing from
  `cards/useDashboard.ts`, which reads only `GET /api/manage/rows` and
  the search; their decisions are `cards/dashboard.ts`, their look
  `cards/dashboardCard.css`. A section card leaves the ground and the
  space below it to the Page it sits in.
- `searchView(opts?: { q? })` — "Unified Search (new)", the friendly search
  (`cards/SearchCard.ce.vue`): a box that takes words and filters, a
  "Meaning only" switch that moves the free text into a `qmd_vsearch:`
  predicate, chips for the sources the results come from with their
  counts (`/search/groups?by=source_ref`), the results as a list with
  the typed words marked, and the picked result drawn in place by
  `documentView` in a shadow root of its own. "View as table" opens
  `gridView` on the same query. The toolbar's search box (⌘K) opens
  one. Its query logic is `cards/search.ts`.

- `gridView(opts?: { q?, columns?, name?, url?, placeholder? })` —
  search bar + a SlickGrid over `/applet/unified_index/search`, or over
  `url`, another table that pages, sorts and groups the way the search
  does (the Manage screen's problems cell opens
  `/applet/unified_index/problems` this way). Each answer says which
  field names a row, which document a row opens and whether qmd ranks
  its free text (`RowsSpec`); the qmd columns and ranking appear only
  for the search. Row click opens the row's document via
  `host.openCards`; double-click opens it as a standalone single-column
  page in a new tab. Persists `q`/`sel`/`cols` state. A search given
  no `q` opens on `is:document`, one row per document; with no
  `placeholder`, the empty bar suggests filters on the biggest source
  the index holds (`cards/searchDefaults.ts`).
- `documentView(markdownUuid?, sectionUuid?)` — renders one document
  (`/applet/unified_index/chat/{markdownUuid}`), highlighting and scrolling to
  `sectionUuid`. A different selection is a different card: the grid
  opens a fresh card rather than mutating an existing one. Shows
  doc-level outgoing edges and decorates span-level edge sources
  (see [`edges.md`](edges.md)); clicking either opens the destination
  via `host.openCards`. The body is drawn in a frame whose policy runs
  no script (`cards/docFrame.ts`). Its links, clicks and selection
  reach the card as events, and the frame scrolls itself.
- `documentPickerView()` — parameter-less gallery stand-in for
  `documentView`: lists every rendered document (`/applet/unified_index/docs`) and
  replaces itself with `documentView("<uuid>")` on pick.
- `aliasView()` — the component library: every custom component in the
  frontend store, by qualified name; a click opens it with its stored
  arguments.
- `dactalView(opts?: { load?, q? })` — DACTAL's query language and table
  UI over a working set of search results; see [`dactal.md`](dactal.md).
- `perseusView()` — a control panel over the Perseus editions: pick
  versions and a locator, and it opens one reader card per version.
- `sourceDagView()` — the sources' step graph, live while a sync runs.
- `umapView(opts?: { q?: string; by?: string })` — the embedding map
  (`cards/UmapCard.ce.vue`, over the applet's `/embedding_map`): every
  document qmd embedded, placed by the `embedding_map` step. The search
  bar takes the grid's grammar and greys out what it does not match
  (`/embedding_map/matches`); a legend colours by one field and
  isolates on hover; hovering a point previews it, clicking opens
  `documentView` beside the card. Persists `q`/`by`/`sel`. The pure
  half — colours, view, hit-testing — is `cards/embeddingMap.ts`.
- `galleryView()` — the new-card gallery (see "Titles, icons and edit mode"
  above); replaces itself with whatever the user picks.
- `agentSeedView(name)` — the in-card hand-off instructions a freshly
  minted, agent-bound component is seeded with (the gallery's agent
  entry stores `() => agentSeedView("<name>")` as the alias source);
  the agent's first save replaces it.
- `tableView({ url })` — the typed table viewer over any endpoint that
  answers `{columns, rows}` (plus `tree: true` when each row carries a
  `path`). See "Typed tables" below.
- `sourcesView(opts?: { add? })` — the Manage screen as a card
  (`cards/SourcesCard.ce.vue`); `add: true` opens it on the add-source
  form, which is what the Dashboard's "Add source" does. It shows the
  tree of what `config.toml` declares over `GET /api/manage/rows`,
  drawn by `TableGrid`, with the row actions and the dialogs they open
  — the wizard, a removal's confirm — teleported to `<body>`. Its
  header says how the last sync went, the config's notices are strips above the table, its status column reads
  word first ("Failed · 2 hours ago"), and its rows follow the density. Through `host.openCards` it opens beside
  itself a `gridView(...)` for Browse or a problems count, a
  `logView(...)` for a step's log or the server's, a `historyView(...)`
  for a row's commit history, a `syncDashboardView(...)` for a group's
  sync, and `configView()`. What a row's actions do is
  `cards/rowActions.ts`, shared with the dashboard; the card keeps only
  what needs the config's text — edit, rename, remove, compare. The
  `/data_sources` route is this card alone at 1.6× width (`MANAGE_STACK`
  in `router/index.ts`). The card's logic is `cards/sourcesCardModel.ts`
  (`useSourcesCard`); the component holds only the template and styles.
- `syncDashboardView({ group, step })` — one group's sync
  (`cards/SyncDashboardCard.ce.vue`): the group's Manage row at the top
  and each step's row under it, laid out vertically, each with a
  toolbar of its row-menu actions and small charts over one run
  (`cards/TimeChart.ce.vue`, drawn by [uPlot](https://github.com/leeoniya/uPlot),
  whose stylesheet the card's `styleSources` carry into its shadow root;
  which charts, and their arithmetic, are `cards/dashboardCharts.ts`). The series are
  `GET /api/manage/groups/{id}/dashboard`: every metric the step
  reported, its warning and error lines counted up, and its tree's
  size, for the newest run the group took part in or the one picked.
  Every chart shares the group's time axis and one crosshair (uPlot's
  cursor sync, keyed per card). The group's
  log for that run is a collapsible `RunLogPanel` at the bottom.
  `step` scrolls to that step's section.
- `logView({ run, step, launch, q, jumpToEnd })` — the run log
  (`components/RunLogPanel.ce.vue` in `cards/LogCard.ce.vue`): one
  process's lines — a step's newest attempt, the runner, a launch of the
  server — or a whole run's, with pickers to move between them and the
  query bar over `/api/log`. The Manage card opens one beside itself for
  a step or for the server. Selecting a line (a click, or the arrow
  keys) opens `logLineView` via `host.openCards`, the way the grid
  opens a document.
- `historyView({ trees, title, source, compare })` — the commit
  history of every doltlite store under some trees
  (`cards/HistoryCard.ce.vue`, over `/api/pipeline/history`): store,
  commit and table as a tree, re-read whenever the runner's record
  moves; a commit's run opens its `logView`. With `source`, two
  commits of that source's download store can be selected and
  compared, which adds a diff group to the config and syncs it;
  `compare: true` opens with the newest two set up. The compare bar
  names each side by the minute it was made and counts what the
  commits between them added, deleted and modified, each summed on
  its own. Every count on
  the card is over the tables that hold records, not datalib's own
  (`datalib_history::holds_records`). The pairing rules are
  `config/compareCommits.ts`.
- `logLineView(seq)` — one log line in full (`cards/LogLineCard.ce.vue`,
  over `/api/log/{seq}`): the message, the fields as a tree
  (`cards/JsonTree.ce.vue`), the source link at the process's commit,
  the process itself. Its *keep* / *exclude* buttons narrow the log
  beside it through the bus (`log.query`, a token for the query bar).
- `configView()` — `config.toml` itself, edited directly, saved through
  the backend's loader. Reloads on the root's `config_changed` frame
  and, a beat sooner, on the `config.written` bus topic a card publishes
  after writing the file.

## Typed tables

A table's producer declares its columns and the viewer draws each cell
by the column's *type* rather than by the field's name. The vocabulary
is `datalib_columns` (`datalib/backend/columns/src/lib.rs`), mirrored
by hand in `datalib/ui/src/api.ts` — change both halves together —
and the one renderer for all of it is `cards/typedColumns.ts`: a pure
function from the declared specs to column definitions, with the cell
renderers every grid shares. Two kinds of host use it.
`cards/TableGrid.ce.vue` is a grid over it for a card that wants a
table and nothing more (`tableView`, the sources card); a card that
drives a grid itself — its own selection, column state in the URL,
adaptive visibility (`GridCard`) — takes its definitions and keeps its
own grid. The split is deliberate: a component that owned the grid
*and* re-exposed its options for the second kind of host would be a
wrapper around a wrapper, with every re-exposed option a place for the
two to disagree.

### The grid, and how to swap it

Every grid is SlickGrid, through `@slickgrid-universal/vanilla-bundle`
(MIT) — the bundle rather than the `slickgrid-vue` wrapper because a
card is a custom element, and the wrapper looks its container up on
`document`, which cannot see into a shadow root; the run log panel
could use the wrapper (it is teleported to `body`) and uses the bundle
anyway, so there is one grid API in the tree. Not AG Grid: the
features the grids use (row grouping, tree data, the context menu) are
in its Enterprise modules, which need a licence, and this repo is
public.

The choice is meant to stay reversible, so the grid is kept behind a
few seams; these are the files that would change if it were swapped
again, and the only ones that should know the grid's DOM or options:

- `cards/typedColumns.ts` — `ColumnSpec` → column definitions.
  `cards/cellRenderers.ts` beneath it is plain DOM and would not change.
- `cards/TableGrid.ce.vue`, `cards/GridCard.ce.vue` (its grid half),
  `components/RunLogPanel.ce.vue`, `components/ProbeItemPicker.vue` — the
  four places a grid is built.
- `grid/menu.ts` and `grid/rowKeys.ts` — the row menu and the `data-key`
  a row carries; `grid/query.ts` knows no grid at all.
- `grid/copyRows.ts` — ⌘C (Ctrl+C) on a grid copies its selected rows
  as TSV, a header line first, in the columns shown; a text selection
  inside one selected row copies as the browser would. Each grid says
  what a cell copies as (`copyText` in `cards/typedColumns.ts` by type).
- `grid/rowHover.ts` — lights both halves of a row that pinned columns
  split in two; the theme's `:hover` reaches only the half under the
  pointer.
- `grid/gridFrame.ts` — how a grid sits in its card: the resize options
  that make it follow the card's frame, and the theme test.
- `grid/columnLayout.ts` — how every grid treats its columns: never
  fitted to the viewport, so a width a person drags stays, and carried
  across a rebuild. Each of the four spreads its `KEEP_COLUMN_WIDTHS`.
- `cards/tableGrid.css` — every `.slick-*` rule; the theme itself comes
  in through `main.ts` and each card's `styleSources`.
- `tests/e2e/grid-helpers.ts` — every selector the specs use to reach a
  grid (`SEARCH_ROWS`, `TABLE_ROWS`, `menuEntry`, `SELECTED_ROWS`, the
  `window.__fwGridApi` hook). A spec that reaches past these is the
  thing to fix, not the grid.

Nothing persisted depends on the grid: the URL's `cols` is the card's
own layout shape and decodes to nothing when it cannot be read.

| type | the cell's value | drawn as |
|---|---|---|
| `text` | a string | as is |
| `count` | an integer | grouped digits |
| `number` | a float | a few decimals, right-aligned |
| `bytes` | an integer | a base-10 size, exact figure on hover |
| `timestamp` | an ISO stamp about *now* (when something last ran) | "7 days ago", exact stamp on hover; sorts on the instant |
| `datetime` | an ISO stamp that is the record's (when a message was sent) | the date and time it names; sorts on the instant |
| `quantity` | `{value, unit, note, detail}` | one figure by its unit (`count` grouped, `seconds` as "25 min"), or `note` in its place — a word, muted — when there is none; the reasoning on hover |
| `timeseries` | `{value, unit, samples, detail, window_secs}` | the value and its change over the window, over a sparkline scaled to its own range; each value names its own window, so minutes of bytes and days of items share a table |
| `identity` | `{id, label, icon, detail, entity}` | icon + label, id on hover; the icon is a *token* (`slack`, `step:ingest`) the viewer maps to an asset. An id that is a person's handle as a URI (`mailto:…`), or an `entity` naming a group or step (`datalib:group/slack`), draws as a chip instead, see below |
| `status` | `{key, label, at, last_success_at, detail}` | a glyph for the key (a spinner while running), when it got there, the reason on hover |
| `chips` | `[{kind, text, title}]` | a row of chips |
| `actions` | `[{id, label, enabled, hint, disabled_reason, danger, on}]` | buttons, or a switch when `on` is set; the card supplies the handler for each id, and an id with no handler draws nothing |
| `markdown_uuid` | a uuid or `{id, label}` | the title; click opens the document |

The producer resolves, the viewer presents: an `identity` arrives with
its label already looked up, because only the producer can, and the
viewer decides what the icon token looks like. An action is an *id*,
never a URL — a URL arriving as data would be a capability.

One layer is joined in the viewer, deliberately: a person. The search
grid's Author cell arrives as an identity whose id is the author's
handle as a URI and whose label is the name the source showed; the grid
draws it as a chip and asks `people`, the one resolver every document
and grid shares (`cards/resolver.ts`, an instance in `cards/contacts.ts`):
a cell asks as it is drawn, the questions of one drawing pass go out as
one request, and when an answer changes — it lands, or a link made in
any document forgets it — every grid and document showing that handle
draws it again. The
producer still resolves what only it can; the contact a person linked
is live state that moves while the row does not, so it is joined where
it is live, the same way a document draws its chips. The reasons, and
what a chip offers on click, are in
[`plans/chips.md`](plans/chips.md) § "In a grid". A group's or a step's
identity resolves the same way, from `entities` over datalib-http's
`POST /api/entities`.

`GET /api/manage/rows` and the `unified_index` applet's `/search` are
the two producers. The applet resolves the search grid's Source
identity itself, from `config.toml` (`applets/src/unified_index/columns.rs`)
— the group's name, led by the configured source's own mark, as the
Manage screen's Name cell is — which is
what keeps renaming a source free of a re-index. The cell styles live
in `cards/tableGrid.css` rather than a component's `<style>`: a
`.ce.vue`'s styles attach to the component for the card adapter to
drop into a shadow root, so every card that draws typed cells passes
the file as a style source.

Adding a view = adding a factory to `ViewLibs` in
`datalib/ui/src/cards/libs/index.ts` (and its name to the
`ViewLibs` type). The factory's job is to capture its arguments and
return a `CardRender`; keep the heavy lifting in a `.ce.vue` component
behind `vueCard`.
