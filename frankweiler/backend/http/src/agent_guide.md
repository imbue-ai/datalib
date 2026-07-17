# Building a frankweiler card (agent guide)

You were pointed here by a "wayfinder" snippet copied out of the
frankweiler UI. It named a **component alias** (e.g. `card_a1b2c3`) and
asked you to define it. This doc tells you how.

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
`description` in the PUT body:

```sh
curl -X PUT "<origin>/api/lib/<aliasName>" \
  -H 'content-type: application/json' \
  -d "$(jq -Rs '{source: ., description: "One line on what this shows."}' < factory.js)"
```

A described component **must work when invoked with no arguments** —
the gallery creates it as `<aliasName>()`. Omitting `description` on a
later PUT keeps the stored one; sending `""` clears it (and removes
the component from the gallery).
