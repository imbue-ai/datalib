# Cards — source-defined cards and the layouts that host them

The datalib UI is a surface of cards: each card is a piece of
JavaScript source the user can read (and edit) in the card's header
bar. The host evaluates that source to produce the card's content.
`datalib/ui/src/cards/types.ts` is the canonical home of every
shape described here; this doc is the narrative version.

A **layout** is what arranges cards on screen — a stack of miller
columns, tabs in a sidebar tree, a 2D tree, a tiling window manager —
selectable from the status bar, and remembered in the browser (see `datalib/ui/src/views/CardsView.vue`). This doc is
deliberately layout-agnostic: it describes the card contract and how a
card interacts with whatever layout hosts it. The layouts differ only
in *where* they put cards and what reshaping furniture they offer;
those specifics live with each layout (`MillerView.vue`,
`TabsView.vue`, `TreeView.vue`, `TilingView.vue`). What every layout guarantees a card
is identical, and is the subject here.

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
expression. An IIFE that composes the factories works today:

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
theming.

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

## Titles and dev mode

The chrome bar around each card has two faces, switched by the **dev**
toggle in the status bar (`datalib/ui/src/devMode.ts`, persisted in
localStorage):

- **Dev mode off** (the default): the bar shows the card's
  human-readable **title** — read-only, no code visible.
- **Dev mode on**: the bar shows the card's source in the editable box
  described below (Enter re-runs the card).

The 🤖 agent hand-off button shows in **both** modes, but only on cards
backed by a user-defined component (the source's callee is an alias in
the `/api/lib` manifest — builtins live in the app bundle, so there's
nothing an agent could modify). Pressing it walks the user through
handing the component to a coding agent (`datalib/ui/src/handoff.ts`
and `components/AgentHandoffModal.vue`): a step list whose first step
copies a "wayfinder" prompt, plus a persisted "skip these steps next
time" opt-out that turns the button into a straight copy.

Card creation is the same gesture in both modes: every layout has an
"add card" affordance (the miller layout's "+" strip after the last
column, the tree layout's "+ card" button, the tiling layout's ＋ add
areas), and it always creates a `galleryView()` card — the **new-card
gallery** (`datalib/ui/src/cards/libs/galleryView.ts`): a list of
every titled component with a short description, builtins first
(sourcesView leading), then every component in the frontend store. A
store entry's row expands to its qualified name called with its stored
`component_args`, so one component appears once per namespace with its
own arguments (`comp.slack_work.channels("slack_work")`,
`comp.slack_personal.channels(…)`) — and a custom component may take
arguments here, unlike a builtin, which still needs a
parameter-less stand-in. Any user-defined component whose `/api/lib`
entry carries a `description` (listed under its stored `title` when it
has one), then a "new component, built by an agent" entry that mints a
fresh alias seeded with `agentSeedView` (the in-card hand-off
instructions) and repoints the card at it. Agents can later rename the
placeholder alias (`POST /api/lib/{name}/rename`); the store leaves a
tombstone and `ShadowCard` rewrites any card still referencing the old
name. In dev
mode the gallery additionally shows each entry's source and a footer
note that source can be typed straight into the chrome bar. Picking an
entry replaces the gallery card with the chosen component via
`host.setSource`. Components that need arguments register a
parameter-less picker instead — `documentView`'s gallery stand-in is
`documentPickerView()`, which lists every rendered document
(`/applet/unified_index/docs`) and replaces itself with `documentView("<uuid>")` on
pick. Cards opened by other cards (`host.openCards`) work the same in
both modes.

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

The grid card retitles itself (`Search: <q>`) as the user searches;
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
  cardId: string;        // host-assigned, stable for the card's lifetime
  initialState: string;  // persisted state from the host ("" when absent)
  setTitle(title: string | null): void;  // chrome-bar title (see above)
  setHelp(html: string | null): void;    // the chrome's "?" (see below)
  bus: Bus;              // ambient cross-card events
  host: HostCommands;    // structural + persistence commands
};
```

### Help

Every card should offer help: what it shows and how to work it, as
HTML, through `ctx.setHelp` — usually right after `setTitle`. The chrome
grows a "?" that opens it in a popup over the page (`CardControls.vue`;
the text is kept per card id in `cards/help.ts`, so all three layouts
share one mechanism). A card with no help is a card that assumes its
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
  **no assumption about placement**: where the new card lands is
  entirely the active layout's business (next to the caller, as a
  child node, as a sibling, …). Structural operations always go
  through host commands, never the bus.
- `hrefFor(...sources)` is the URL `openCards(...sources)` would land
  on, so a card can draw a **real link**: a plain click goes through
  `openCards`, and a modified click, a middle click, the context menu
  and a drag are the browser's — a new tab, a copied link, a bookmark.
  The document card's edge list is the pattern (`onEdgeLinkClick`;
  `isBrowserClick` in `cards/chatLink.ts` is the one rule for which
  clicks to leave alone). A layout the URL does not describe answers
  with the chain alone.
- `close()` closes this card. A layout may close dependents along with
  it (e.g. a node's subtree) — that's its call, not the card's.
- `setState(state)` replaces this card's persisted state string (see
  below).

### State strings

A card may persist state so it survives a re-run (and, where the
layout supports it, a reload). The string is **opaque to the host**:
the card passes whatever it likes to `setState`, the host round-trips
it, and the card reads it back as `ctx.initialState`. Setting `""`
clears it. How long it survives is the layout's choice — a
URL-backed layout persists it across reloads; an in-memory layout
keeps it only until the page is gone — so a card must treat
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
receiver. It carries no structural operations. The only topic today is
`edge.hover` (`TOPIC_EDGE_HOVER`): a document card publishes the
destination of the edge under the cursor
(`{ markdownUuid, sectionUuid } | null`), and every document card
subscribes, matching `markdownUuid` against its own doc to put a
transient highlight on the target span. Payloads cross card boundaries
as `unknown`; subscribers validate the shape before acting.
Unsubscribe in the card's teardown.

## How a card and its layout interact

Everything a card needs from its surroundings is the `CardCtx`; it
never reaches for the layout directly. The division of labour:

- **The card** owns its shadow root, renders into it, persists its own
  opaque state, and asks for new cards / closure through host commands.
- **The layout** owns placement and chrome. Around each card it draws
  a header with the source box (Enter re-runs the card, Shift+Enter
  inserts a newline; committing new source clears the old state
  string), an ↗ "open this card alone" link — a new browser tab, or in
  the desktop app a second native window of the app (the shell's
  `on_new_window` handler in `datalib/tauri/src/main.rs`), so a card
  can live on another screen — and a ✕ close button. There is no back
  or forward in the chrome: in the miller layout every `setSource` — a
  gallery pick, an agent hand-off, a source edit — and every open and
  close is a browser history entry, and the browser's own Back walks
  them (the desktop app answers ⌘[ / ⌘] and draws the two buttons in
  its toolbar). See "The miller layout and the browser" below.
  Anything past that — resize handles, drag grips, add buttons,
  dividers, tab bars — is layout-specific furniture, invisible to the
  card. The layout also decides what `openCard` placement means, what
  `close` takes with it, and whether `setState` reaches the URL.

Because the contract is the same everywhere, the same card source runs
unchanged in any layout, and a layout can be added or changed without
touching cards. Cards are **not** carried across when the user toggles
layouts — each layout keeps its own set, all kept alive across toggles
so switching back doesn't lose them.

## The miller layout and the browser

The miller stack **is the URL** (`router/columns.ts`), and the browser's
history is the only history. `MillerView` writes it two ways: opening,
closing or repointing a column is a navigation (`push`), so Back undoes
it and Forward redoes it; a card's state and a column's width rewrite
the current entry (`replace`), the way a page's scroll position is not
somewhere Back returns to. Writes go through one queue — the router
cancels a navigation another one overtakes, and a grid row click is two
writes in one tick, the selection then the document. When the URL
changes under the layout (Back, a link, a hand-edited address) the
stack is *reconciled*, not rebuilt: a column whose code and state match
the URL at its position stays mounted, so Back from a document leaves
the grid beside it as it was, still on the row that opened the
document. `views/millerStack.ts` holds these decisions as pure
functions; `document.title` names the stack, newest column first, so a
tab, a history menu and a bookmark say what it is. A `/chat/<uuid>`
link — the shape every renderer writes into a document body — is
routed to that document alone (`router/index.ts`), so the tab a
modified click opens shows the document. The tree and tiling layouts
are in memory only and get none of this. Only the layout on screen
reads or writes the URL; one switched back to puts its own stack back.

## The tabs layout

One card at a time, full size, beside a sidebar listing every open
card as a tree: each tab sits under the tab that opened it, as in
Firefox's Tree Style Tab. `views/tabTree.ts` holds the decisions as
pure functions. Each window has a tree of its own
(`views/tabsWindow.ts`): a window popped out with a tab's or a card's
↗ starts with that card alone, a stack of its own. The first window
open is the main one; it also saves its tree to `localStorage`, and
that is what the next launch restores. The URL names only the
selected tab, as a one-column stack, so a copied link opens that card
alone; the history entry also carries the tab's id, so Back finds the
tab even after its state has moved on. A URL naming a card no tab
shows opens a new root tab (a miller link of several columns opens
as a spine).

A card a card opened is a **preview** (italic in the sidebar) until
the person visits it from the sidebar or it opens something itself;
the next card its opener opens replaces it. Without that, clicking
down a grid's rows would leave a tab per row. Closing a tab hands its
children to its own parent; closing a collapsed one closes its whole
branch. A row's ⤒ makes the tab top-level, taking what is under it
along, and its ↗ opens the tab alone in a new browser tab (a new
window in the app).

## Prebuilt views

The factories in `ViewLibs` are the public surface card source
programs against:

- `gridView(opts?: { q?: string })` — search bar + a SlickGrid over
  `/applet/unified_index/search`. Row click opens the row's document via
  `host.openCard`; double-click opens it as a standalone
  single-column page in a new tab. Persists `q`/`sel`/`cols` state.
- `documentView(markdownUuid?, sectionUuid?)` — renders one document
  (`/applet/unified_index/chat/{markdownUuid}`), highlighting and scrolling to
  `sectionUuid`. A different selection is a different card: the grid
  opens a fresh card rather than mutating an existing one. Shows
  doc-level outgoing edges and decorates span-level edge sources
  (see `docs/dev/edges.md`); clicking either opens the destination via
  `host.openCard`.
- `documentPickerView()` — parameter-less gallery stand-in for
  `documentView`: lists every rendered document (`/applet/unified_index/docs`) and
  replaces itself with `documentView("<uuid>")` on pick.
- `galleryView()` — the new-card gallery (see "Titles and dev mode"
  above); replaces itself with whatever the user picks.
- `agentSeedView(name)` — the in-card hand-off instructions a freshly
  minted, agent-bound component is seeded with (the gallery's agent
  entry stores `() => agentSeedView("<name>")` as the alias source);
  the agent's first save replaces it.
- `tableView({ url })` — the typed table viewer over any endpoint that
  answers `{columns, rows}` (plus `tree: true` when each row carries a
  `path`). See "Typed tables" below.
- `sourcesView()` — the Manage screen as a card: the tree of what
  `config.toml` declares over `GET /api/manage/rows`, drawn by
  `TableGrid`, with the row actions and the panels they open — the
  wizard, a group's commit history — teleported to `<body>`. Browse
  opens a `gridView(...)` beside it through `host.openCards`, and a
  step's log or the server's a `logView(...)` the same way. The
  `/data_sources` route is this card at 1.6× width
  with `configView()` beside it (`MANAGE_STACK` in `router/index.ts`).
- `logView({ run, step, launch, q, jumpToEnd })` — the run log
  (`components/RunLogPanel.ce.vue` in `cards/LogCard.ce.vue`): one
  process's lines — a step's newest attempt, the runner, a launch of the
  server — or a whole run's, with pickers to move between them and the
  query bar over `/api/log`. The Manage card opens one beside itself for
  a step or for the server. Selecting a line (a click, or the arrow
  keys) opens `logLineView` via `host.openCards`, the way the grid
  opens a document.
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
*and* re-exposed the grid's options for the second kind of host was a
wrapper around a wrapper, and every option it re-exposed was a place
for the two to disagree.

### The grid, and how to swap it

Every grid is SlickGrid, through `@slickgrid-universal/vanilla-bundle`
(MIT) — the bundle rather than the `slickgrid-vue` wrapper because a
card is a custom element, and the wrapper looks its container up on
`document`, which cannot see into a shadow root; the run log panel
could use the wrapper (it is teleported to `body`) and uses the bundle
anyway, so there is one grid API in the tree. AG Grid was here until
2026-09-17; its Enterprise modules (row grouping, tree data, the side
bar, the context menu) needed a licence, and this repo is public.

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
| `bytes` | an integer | a base-10 size, exact figure on hover |
| `timestamp` | an ISO stamp | "7 days ago", exact stamp on hover; sorts on the instant |
| `timeseries` | `{value, unit, samples, detail}` | the value over a sparkline, calibrated across the column |
| `identity` | `{id, label, icon, detail}` | icon + label, id on hover; the icon is a *token* (`slack`, `step:ingest`) the viewer maps to an asset |
| `status` | `{key, label, at, detail, fraction, segments}` | a glyph for the key, the reason on hover, a bar while running |
| `chips` | `[{kind, text, title}]` | a row of chips |
| `actions` | `[{id, label, enabled, disabled_reason, danger}]` | buttons; the card supplies the handler for each id, and an id with no handler draws nothing |
| `markdown_uuid` | a uuid or `{id, label}` | the title; click opens the document |

The producer resolves, the viewer presents: an `identity` arrives with
its label already looked up, because only the producer can, and the
viewer decides what the icon token looks like. An action is an *id*,
never a URL — a URL arriving as data would be a capability.

`GET /api/manage/rows` and the `unified_index` applet's `/search` are
the two producers. The applet resolves the search grid's Provider and
Source identities itself, from `config.toml` (`applets/src/unified_index/columns.rs`)
— the configured source's own mark and the group's name — which is
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
