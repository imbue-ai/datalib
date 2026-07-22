# Building a frankweiler card (agent guide)

You were pointed here by a "wayfinder" snippet copied out of the
frankweiler UI. It named a **component alias** (e.g. `card_a1b2c3`) and
asked you either to define it (a new component) or to modify an
existing one. This doc tells you how. (If your wayfinder is about the
data-source config instead, read `<origin>/agent/config.md`.)

## The model

The UI is a stack of columns; each column is a **card** defined by a
small JS expression (its "source"). The source is evaluated with a set
of **view factories** in scope and must return a `CardRender`:

```js
// a CardRender owns a shadow root and returns a teardown
type CardRender = (root: ShadowRoot, ctx: CardCtx) => (() => void);
```

A **component alias** is a named, reusable view factory — a function
that takes arguments and returns a `CardRender`, exactly like the
builtin `gridView` / `documentView`:

```js
// the value an alias must evaluate to: a factory
(args) => (root, ctx) => {
  root.innerHTML = "<h1>hello</h1>";
  return () => {};            // teardown: undo anything global
};
```

The card that points at your alias has source `<aliasName>()`. Whenever
you overwrite the alias, that card re-renders automatically.

## What you do

1. **Write a factory.** Either inline JS (an expression that evaluates
   to a factory function), or — for anything non-trivial — author it in
   TypeScript with npm deps and **bundle to a single ES-expression**
   (e.g. `esbuild app.ts --bundle --format=iife --minify` wrapped so the
   whole thing evaluates to the factory).

   The stored source is evaluated as `return (<source>)`, so it must be
   exactly **one expression**: no statements, no module
   `import`/`export`, and **no trailing semicolon** — end with `… }`,
   never `… };`.

   When the wayfinder asks you to **modify** an existing component,
   start from its current source: `GET <origin>/api/lib/<aliasName>`
   returns it.

2. **Save it** under the alias the wayfinder gave you:

   ```sh
   curl -X PUT "<origin>/api/lib/<aliasName>" \
     -H 'content-type: application/json' \
     -d "$(jq -Rs '{source: .}' < factory.js)"
   ```

   `<origin>` is the base URL in your wayfinder (e.g.
   `http://127.0.0.1:5173`). Re-PUT to iterate; each PUT live-reloads the
   card.

3. **Look at the result.** The card lives at the column URL in your
   wayfinder. Render it headlessly and inspect the screenshot:

   ```sh
   node frankweiler/ui/scripts/render.mjs '<cardUrl>' --out /tmp/card.png
   # prints JSON: { consoleErrors, cardErrors } — check these for failures
   ```

   Open `/tmp/card.png` to see what the user sees. Iterate on 1–3 until
   it looks right.

## Rules for the factory

- **Shadow DOM isolation.** Your `root` is a shadow root. Document-head
  CSS does not reach it — inject any styles into `root` yourself. The
  app's `--fw-*` theme CSS custom properties *do* inherit across the
  boundary; use them to pick up theming.
- **Return a teardown.** Remove listeners/intervals you added globally.
- **Set a title.** Call `ctx.setTitle("…")` first thing in your render
  (and again if a better title emerges later, e.g. after a fetch) —
  it's what the card's chrome bar shows outside dev mode. Skipping it
  falls back to the alias name.
- **Other aliases are in scope** by name: if your factory references
  another alias `bar`, that's a live dependency and the card re-renders
  when `bar` changes too. The builtins `gridView` and `documentView` are
  always in scope.
- **Data** comes from the backend HTTP API (same origin): e.g.
  `GET /api/search?q=…`, `GET /api/chat/{markdown_uuid}`. Fetch with
  relative paths.

## Composing existing components

```js
// an alias that wraps the builtin grid, pre-filtered
(q) => gridView({ q })
```

## Publishing to the new-card gallery

The UI's "new card" gallery lists parameter-less components with a
short description. To make your component appear there, include a
`description` — and a human-readable `title`, which listings show
instead of the bare alias name — in the PUT body:

```sh
curl -X PUT "<origin>/api/lib/<aliasName>" \
  -H 'content-type: application/json' \
  -d "$(jq -Rs '{source: ., title: "Nice name", description: "One line on what this shows."}' < factory.js)"
```

A described component **must work when invoked with no arguments** —
the gallery creates it as `<aliasName>()`. Omitting `title` /
`description` on a later PUT keeps the stored values; sending `""`
clears one (clearing the description removes the component from the
gallery).

## Giving your component a real name

The wayfinder hands you a placeholder name like `card_a1b2c3`. Once
the component works, rename it to something meaningful:

```sh
curl -X POST "<origin>/api/lib/card_a1b2c3/rename" \
  -H 'content-type: application/json' \
  -d '{"new_name": "myNiceName"}'
```

The new name must be a valid JS identifier (≤64 ASCII chars), must not
already be taken (409), and must not be one of the builtin view names
(`gridView`, `documentView`, …). Cards that still reference the old
name repoint themselves automatically — the store leaves a redirect
behind, and the UI rewrites card source when it sees it. Rename last,
after your final PUT: further saves must target the new name.
