# Cards — source-defined miller columns

The frankweiler UI is a stack of miller columns, and every column IS a
card: a piece of JavaScript source the user can read (and edit) in the
column's header bar. The host evaluates that source to produce the
column's content. `frankweiler/ui/src/cards/types.ts` is the canonical
home of every shape described here; this doc is the narrative version.

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
(`frankweiler/ui/src/cards/libs/index.ts`), plus JS globals. The
expression must evaluate to a `CardRender`; anything else (or a parse
error) renders as an error message inside the column instead of a
card.

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

The host (`frankweiler/ui/src/views/MillerView.vue`, via
`frankweiler/ui/src/components/ShadowCardColumn.vue`) mounts each card
inside its own **shadow root** and calls the render function with it.
The card owns that DOM completely — the host renders nothing inside.
The returned teardown runs when the column closes or its source is
re-run after an edit.

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

## CardCtx: what a card receives

```ts
type CardCtx = {
  cardId: string;        // host-assigned, stable for the column's lifetime
  initialState: string;  // persisted state from the URL ("" when absent)
  bus: Bus;              // ambient cross-card events
  host: HostCommands;    // structural + persistence commands
};
```

### HostCommands

Each card gets its own instance, pre-bound to its column:

```ts
type HostCommands = {
  openColumn(source: string): string;  // returns the new card's id
  close(): void;
  setState(state: string): void;
};
```

- `openColumn(source)` opens a new column directly to the right of
  this card, **replacing everything currently further right** (miller
  semantics). The argument is card source for the new column — e.g.
  the grid card composes `documentView("<md>", "<row>")` when a row is
  clicked. Structural operations always go through host commands,
  never the bus.
- `close()` closes this card's column.
- `setState(state)` replaces this card's persisted state string (see
  below).

### State strings

A card may persist state across reloads. The string is **opaque to
the host**: it lands verbatim in the column's URL segment, comes back
as `ctx.initialState` on the next load, and only the card interprets
it. Setting `""` clears it.

The grid card is the reference user
(`frankweiler/ui/src/cards/GridCard.ce.vue`): it keeps
`URLSearchParams` of `q` (search query), `sel` (selected row uuid) and
`cols` (AG Grid column state, base64url-encoded JSON), writing only on
user-driven changes so a pristine grid keeps a clean URL.

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

## URL scheme

The URL path is the column stack
(`frankweiler/ui/src/router/columns.ts`): a `/`-separated list of
`code:state` segments, one per column, with both parts
percent-escaped (so `/` and `:` inside them survive — the first raw
`:` in a segment is the separator):

```
/gridView():sel%3D11ec…%26cols%3DW3si…/documentView(%22e28e…%22)
```

- `code` and `code:` are equivalent: a column whose state is empty
  serializes as bare code.
- `/` is the pristine default stack (`[gridView()]` with no state).
- Reloading restores the stack and each card's state; back/forward
  rebuilds the stack from the path.
- The trailing blank column (below) is never part of the URL.
- Each column's ↗ header button links to a URL containing just that
  column — "open this column alone".

## Host chrome

Around each card, the host draws the header bar and nothing else. The
header holds the source box (soft-wrapping; Enter re-runs the card,
Shift+Enter inserts a newline; committing new source clears the old
state string), the ↗ open-alone link, and the ✕ close button. Columns
resize by dragging the invisible strip on their right divider.

Invariant: the stack always ends in **exactly one blank column** —
the place to type new card source. As soon as it gains code, a fresh
blank appears after it; a run of several trailing blanks collapses to
one.

## Prebuilt views

The factories in `ViewLibs` are the public surface card source
programs against:

- `gridView(opts?: { q?: string })` — search bar + AG Grid over
  `/api/search`. Row click opens the row's document via
  `host.openColumn`; double-click opens it as a standalone
  single-column page in a new tab. Persists `q`/`sel`/`cols` state.
- `documentView(markdownUuid?, sectionUuid?)` — renders one document
  (`/api/chat/{markdownUuid}`), highlighting and scrolling to
  `sectionUuid`. A different selection is a different card: the grid
  opens a fresh column rather than mutating an existing one. Shows
  doc-level outgoing edges and decorates span-level edge sources
  (see `docs/edges.md`); clicking either opens the destination via
  `host.openColumn`.

Adding a view = adding a factory to `ViewLibs` in
`frankweiler/ui/src/cards/libs/index.ts` (and its name to the
`ViewLibs` type). The factory's job is to capture its arguments and
return a `CardRender`; keep the heavy lifting in a `.ce.vue` component
behind `vueCard`.
