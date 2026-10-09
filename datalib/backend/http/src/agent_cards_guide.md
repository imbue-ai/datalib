# Building a datalib card (agent guide)

You were pointed here by a "wayfinder" snippet copied out of the
datalib UI. It named a **component alias** (e.g. `card_a1b2c3`) and
asked you either to define it (a new component) or to modify an
existing one. This doc tells you how. (Editing the data-source config
instead? Read `<origin>/agent/config.md`.)

## Authentication (do this first)

Every `/api/*` route requires the server's API token — the same scheme
Jupyter uses, and for the same reason: without it any web page the user
has open could drive this API. (This guide itself is public, so you can
read it before you have the token.)

The token is minted per server process and published to a file:

```sh
TOKEN=$(cat <data root>/system/api-token)
curl -H "Authorization: Bearer $TOKEN" "<origin>/api/health"
```

Your wayfinder names the exact path. **Read it fresh** rather than
caching it: a 401 from any call below almost always means the server
restarted and minted a new one.

## The model

The UI is a stack of **cards**. Each card has a **card source**: one
JavaScript expression, evaluated as `return (<source>)` with the builtin
view factories (`gridView`, `documentView`, …) and `comp` in scope. It
must produce a `CardRender`:

```js
// a CardRender owns a shadow root and returns a teardown
type CardRender = (root: ShadowRoot, ctx: CardCtx) => (() => void);
```

A **component** is a named factory — a function that takes arguments
and returns a `CardRender`. Yours live in the `user` namespace, and a
card calls one as `comp.user.<aliasName>(…)`. The card your wayfinder
points at has source `comp.user.<aliasName>()`, and it re-renders
whenever you save the component.

A component is stored as an **ES module whose default export is the
factory**:

```js
export default (args) => (root, ctx) => {
  root.innerHTML = "<h1>hello</h1>";
  return () => {};            // teardown: undo anything global
};
```

## What you do

1. **Write the module.** Plain JS is fine; for anything non-trivial,
   author it in TypeScript with npm deps and bundle it to one
   self-contained ES module (e.g. `esbuild app.ts --bundle
   --format=esm --minify`). It is served from a flat store, so a
   relative `import` cannot resolve.

   When the wayfinder asks you to **modify** an existing component,
   start from its current source: `GET <origin>/api/lib/<aliasName>`
   returns it.

2. **Save it** under the alias the wayfinder gave you:

   ```sh
   curl -X PUT "<origin>/api/lib/<aliasName>" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' \
     -d "$(jq -Rs '{source: .}' < component.js)"
   ```

   `<origin>` is the base URL in your wayfinder. Re-PUT to iterate;
   each PUT live-reloads the card.

3. **Look at the result.** The card lives at the column URL in your
   wayfinder. Render it headlessly and inspect the screenshot (this
   needs a datalib checkout with `pnpm install` run in `datalib/ui`):

   ```sh
   node datalib/ui/scripts/render.mjs '<cardUrl>' --out /tmp/card.png \
     --token "$TOKEN"
   # prints JSON: { url, out, consoleErrors, cardErrors }; exits non-zero if either is non-empty
   ```

   Open `/tmp/card.png` to see what the user sees. Iterate on 1–3 until
   it looks right.

## Rules for the component

- **It sees only browser globals.** The module is imported, not
  evaluated as card source: `gridView`, `comp` and other components are
  not in scope inside it.
- **Shadow DOM isolation.** Your `root` is a shadow root. Document-head
  CSS does not reach it — inject any styles into `root` yourself. The
  app's `--datalib-*` theme CSS custom properties *do* inherit across the
  boundary; use them to pick up theming.
- **Return a teardown.** Remove listeners/intervals you added globally.
- **Set a title.** Call `ctx.setTitle("…")` first thing in your render
  (and again if a better title emerges later, e.g. after a fetch) —
  it's what the card's chrome bar shows outside edit mode. Skipping it
  falls back to one derived from the card source.
- **Offer help.** `ctx.setHelp("<p>…</p>")` puts a "?" on the card
  that opens what this card shows and how to work it.
- **Data** comes from the backend HTTP API (same origin): e.g.
  `GET /applet/unified_index/search?q=…`, `GET /applet/unified_index/chat/{markdown_uuid}`. Fetch with
  relative paths — and *don't* add an auth header in component code. The
  browser holds the token as an HttpOnly session cookie it sends
  automatically; the token is deliberately not reachable from page JS.

A pre-filtered grid needs no component at all: a card whose source is
`gridView({ q: "…" })` is one.

## Publishing to the new-card gallery

The UI's "new card" gallery lists every component that has a `title`.
Include a `title` and a one-line `description` in the PUT body:

```sh
curl -X PUT "<origin>/api/lib/<aliasName>" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d "$(jq -Rs '{source: ., title: "Nice name", description: "One line on what this shows."}' < component.js)"
```

The gallery calls the component with the PUT's `component_args` (a
JSON array, default `[]`), so a listed component **must work when
called with those arguments** — with none, unless you set them.
Omitting `title`, `description` or `component_args` on a later PUT
keeps the stored value; sending `""` clears a title or description
(clearing the title takes the component out of the gallery).

Add an `icon` to the same body and the app draws it beside the card's
name, in the gallery and in every layout. It is one of:

- a glyph name: `home`, `sources`, `search`, `table`, `map`, `log`,
  `code`, `document`, `history`, `dag`, `dashboard`, `add`, `problem`,
  `chat`, `book`, `component`;
- a source's mark: `slack`, `gmail`, `claude`, `chatgpt`, … (any source
  the wizard offers);
- an image of your own, as a `data:` URL — PNG, JPEG, GIF, WebP or SVG
  (`"data:image/svg+xml;base64,…"`), drawn at 14–18px.

Without one the card gets the generic component glyph. The same
keep/clear rules apply: omit it to keep the stored icon, send `""` to
clear it.

## Giving your component a real name

The wayfinder hands you a placeholder name like `card_a1b2c3`. Once
the component works, rename it to something meaningful:

```sh
curl -X POST "<origin>/api/lib/card_a1b2c3/rename" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"new_name": "myNiceName"}'
```

The new name must be a valid JS identifier (≤64 ASCII chars) and must
not already be taken (409). Cards that still reference the old name
repoint themselves automatically — the store leaves a redirect behind,
and the UI rewrites card source when it sees it. Rename last, after
your final PUT: further saves must target the new name.
