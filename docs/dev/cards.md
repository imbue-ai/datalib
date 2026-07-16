# Cards — source-defined cards and the layouts that host them

The frankweiler UI is a surface of cards: each card is a piece of
JavaScript source the user can read (and edit) in the card's header
bar. The host evaluates that source to produce the card's content.
`frankweiler/ui/src/cards/types.ts` is the canonical home of every
shape described here; this doc is the narrative version.

A **layout** is what arranges cards on screen — a stack of miller
columns, a 2D tree, a tiling window manager — selectable from the
status bar (see `frankweiler/ui/src/views/CardsView.vue`). This doc is
deliberately layout-agnostic: it describes the card contract and how a
card interacts with whatever layout hosts it. The layouts differ only
in *where* they put cards and what reshaping furniture they offer;
those specifics live with each layout (`MillerView.vue`,
`TreeView.vue`, `TilingView.vue`). What every layout guarantees a card
is identical, and is the subject here.

## Card source

A card's source is a JS **expression**, e.g.

```js
gridView()
documentView("e28ed67d-507b-5319-8732-00e249b6ebf6")
documentView("e28ed67d-…", "11ec65e9-…")   // doc + section to highlight
```

`compileCardSource` (`frankweiler/ui/src/cards/cardSource.ts`) wraps
the expression in `new Function(...viewLibNames, "return (<source>)")`
and calls it with the view factories as arguments — so the only names
in scope are the factories in `ViewLibs`
(`frankweiler/ui/src/cards/libs/index.ts`), the helpers in
`scopeHelpers` (today just `titled`, see "Titles and dev mode"), plus
JS globals. The
expression must evaluate to a `CardRender`; anything else (or a parse
error) renders as an error message in place of the card.

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
root** (via `frankweiler/ui/src/components/ShadowCard.vue`) and calls
the render function with it. The card owns that DOM completely — the
host renders nothing inside. The returned teardown runs when the card
closes or its source is re-run after an edit.

Shadow DOM is the isolation boundary: document-head styles do not
reach inside, so a card must inject any CSS it needs into `root`
itself. CSS custom properties (the app's `--fw-*` theme variables) do
inherit across the boundary and are the supported way to pick up
theming.

The prebuilt cards are Vue components, adapted to this contract by
`vueCard` (`frankweiler/ui/src/cards/vueCard.ts`): it injects the
component's compiled styles into the shadow root, mounts a dedicated
Vue app with the ctx as a prop, and returns `app.unmount` as the
teardown. Card components use the `*.ce.vue` suffix so
`@vitejs/plugin-vue` compiles them in custom-element mode, which
attaches their `<style>` blocks as `component.styles` (strings)
instead of injecting them into the document head. Child components of
a card must also be `.ce.vue` and listed in the adapter's
`styleSources` so their CSS lands in the root too (see
`frankweiler/ui/src/cards/libs/documentView.ts` for the pattern).

## Titles and dev mode

The chrome bar around each card has two faces, switched by the **dev**
toggle in the status bar (`frankweiler/ui/src/devMode.ts`, persisted in
localStorage):

- **Dev mode off** (the default): the bar shows the card's
  human-readable **title** — read-only, no code visible.
- **Dev mode on**: the bar shows the card's source in the editable box
  described below (Enter re-runs the card), plus the 🤖 agent hand-off
  button (which rewrites the source, so it's hidden with it).

A card declares its title by wrapping its render in `titled()`
(`frankweiler/ui/src/cards/title.ts`):

```ts
export function gridView(opts?: { q?: string }): CardRender {
  const q = opts?.q ?? "";
  return titled(q ? `Search: ${q}` : "Search", vueCard(GridCard, { q }));
}
```

`titled` just sets the render's optional `cardTitle` property, so it
works the same for builtin factories and user-defined aliases — the
title is computed at factory-call time and can reflect the arguments.
After compiling a source, `ShadowCard` reports the declared title up to
the layout, which shows it in the chrome. A card without one gets a
best-effort fallback (`displayTitle`): the bare factory/alias name for
`name(...)`-shaped source, `new card` for a blank card, or a generic
label for anything else.

## CardCtx: what a card receives

```ts
type CardCtx = {
  cardId: string;        // host-assigned, stable for the card's lifetime
  initialState: string;  // persisted state from the host ("" when absent)
  bus: Bus;              // ambient cross-card events
  host: HostCommands;    // structural + persistence commands
};
```

### HostCommands

Each card gets its own instance, pre-bound to that card:

```ts
type HostCommands = {
  openCard(source: string): string;  // returns the new card's id
  close(): void;
  setState(state: string): void;
};
```

- `openCard(source)` opens a new card "from" this one. The card
  supplies only the new card's **source** — e.g. the grid card
  composes `documentView("<md>", "<row>")` when a row is clicked — and
  makes **no assumption about placement**: where the new card lands is
  entirely the active layout's business (next to the caller, as a
  child node, as a sibling, …). Structural operations always go
  through host commands, never the bus.
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
(`frankweiler/ui/src/cards/GridCard.ce.vue`): it keeps
`URLSearchParams` of `q` (search query), `sel` (selected row uuid) and
`cols` (AG Grid column state, base64url-encoded JSON), writing only on
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
  string), an ↗ "open this card alone" link, and a ✕ close button.
  Anything past that — resize handles, drag grips, add buttons,
  dividers, tab bars — is layout-specific furniture, invisible to the
  card. The layout also decides what `openCard` placement means, what
  `close` takes with it, and whether `setState` reaches the URL.

Because the contract is the same everywhere, the same card source runs
unchanged in any layout, and a layout can be added or changed without
touching cards. Cards are **not** carried across when the user toggles
layouts — each layout keeps its own set, all kept alive across toggles
so switching back doesn't lose them.

## Prebuilt views

The factories in `ViewLibs` are the public surface card source
programs against:

- `gridView(opts?: { q?: string })` — search bar + AG Grid over
  `/api/search`. Row click opens the row's document via
  `host.openCard`; double-click opens it as a standalone
  single-column page in a new tab. Persists `q`/`sel`/`cols` state.
- `documentView(markdownUuid?, sectionUuid?)` — renders one document
  (`/api/chat/{markdownUuid}`), highlighting and scrolling to
  `sectionUuid`. A different selection is a different card: the grid
  opens a fresh card rather than mutating an existing one. Shows
  doc-level outgoing edges and decorates span-level edge sources
  (see `docs/dev/edges.md`); clicking either opens the destination via
  `host.openCard`.

Adding a view = adding a factory to `ViewLibs` in
`frankweiler/ui/src/cards/libs/index.ts` (and its name to the
`ViewLibs` type). The factory's job is to capture its arguments and
return a `CardRender`; keep the heavy lifting in a `.ce.vue` component
behind `vueCard`.
