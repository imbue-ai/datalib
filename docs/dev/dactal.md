# DACTAL view

`dactalView()` is a card view that queries your `grid_rows` with
[DACTAL](https://dactal.org)'s query language and renders the results in
DACTAL's tabular UI. It sits in the card registry alongside
`gridView`/`documentView` and does **not** replace the grid homepage — open it
in any card, e.g. in an empty card's header type:

```
dactalView()                                      # explore recent rows
dactalView({ load: "provider:slack", q: "rows/channel" })
```

`opts.load` is a Datalib search that seeds the working set; `opts.q` is the
initial DACTAL query. Both flow to the page as `?fw=`/`?dq=`.

It is **data-agnostic**: there is no per-provider code. The page loads whatever
`/api/search` returns and converts it to DACTAL datasets on the fly (`bridge.js`),
so it works over any corpus — Slack, GitHub, Notion, Perseus, all of it.

## What DACTAL is

A single-author, dependency-free, **client-side** data explorer distributed as
three classic (non-module) browser scripts, vendored under
`datalib/ui/public/dactal/vendor/` (see `vendor/PROVENANCE.md`):

| File | Role |
|---|---|
| `dactal.js` | engine: `class DACTAL` — `load()`, `parse()`/`executeq()`, grouping/annotators. Ends with `window.DACTAL = new DACTAL()`. |
| `dactal_utils.js` | UI: `buildView()`/`arrayToTable()`/`render()`, the `renderers` registry, `dactal_css`. Tables, heatmaps, tag-clouds, drill-down. |
| `dactal_storage_indexeddb.js` | `class DACTALdb` — IndexedDB persistence (datasets, saved queries, history). |

### Data model
DACTAL holds **named datasets**, each an array of item objects. An item is keyed
by `id`, labelled by `name`; every other field is a property you can follow,
filter, group, or sort by. Items reference each other by `id`: if a dataset named
`author` exists and a row has `author: "qi"`, then `rows.author` **joins** to the
author entity (the `autoresolve` feature).

### Query language
A query starts with a dataset name and chains operators left-to-right:

| Op | Meaning | Example |
|---|---|---|
| `.` | follow a property | `rows.author` |
| `:` | filter | `rows:source=slack` |
| `/` | group | `rows/source` |
| `#` | sort (`-` = descending) | `rows#-when` |
| annotators | `count`, `total`, `average`, `min`, `max`, … | `rows/author.count` |

They compose: `rows:source=slack/channel`, `rows.author.team` (row → author
entity → its team, a two-hop join). Values containing spaces/parens must be
bracketed: `rows:kind=[Slack Message]` (DACTAL treats space/`(`/`)` as syntax).

## How it's wired

```
dactalView() card ─► iframe ─► /dactal/index.html
                                   │
/api/search ──► bridge.js ──► DACTAL engine ──► buildView() table UI
(grid_rows)     rows→datasets    executeq()      drill-down re-runs runQuery
                + survey()
```

- **App glue** (`datalib/ui/src/cards/`): `libs/dactalView.ts` (the factory),
  registered in `libs/index.ts`, typed in `types.ts`, and advertised in the
  empty-card hints in `components/ShadowCard.vue`.
- **Served page** (`datalib/ui/public/dactal/`):
  - `bridge.js` — the **only** Datalib-specific glue: maps each `grid_rows`
    row to a DACTAL item (`uuid`→`id`) and re-normalizes the facet columns
    (`author`, `channel`, `source`, `account`, `project`, `conversation`, …) into
    id-keyed entity datasets so DACTAL's relational joins light up on top of the
    denormalized table. It calls `dactal.survey()` after loading — required, or
    `autoresolve` never fires and `rows.author` stays a bare string.
  - `index.html` — the explorer page loaded in the iframe. Two inputs: a
    Datalib search (pulls a working set into the browser) and a DACTAL query
    over it. Reuses DACTAL's engine + renderer but not its host page (no
    saved-query store / AI assist / adapters). Also carries the CSP — see
    caveat 5.
  - `main.js` — that page's own logic (query bar, render loop, the globals
    the vendored renderer expects). Split out of `index.html` so the CSP
    can forbid inline script.
  - `vendor/` — the three pinned DACTAL scripts.

`public/**` is already in `datalib/ui/BUILD.bazel`'s `vite_inputs`, so the
page ships in packaged builds with no extra wiring; the embedded server serves
`/dactal/index.html` the same as vite dev.

### Why an iframe (not a `vueCard`)
DACTAL ships as classic scripts that attach to `window` globals, assume a single
engine instance per page, and emit inline `onclick=` handlers that resolve
against the top-level window. Mounting that into a card's Shadow DOM would break
the inline handlers and cap us at one DACTAL card per app (shared globals). The
iframe gives each card its own window/engine/IndexedDB, which is what that
reasoning was about. It is **not a security boundary**: the frame has no
`sandbox` attribute and is served from the app's own origin, so its scripts can
reach `/api/*` directly, same-origin, with the session cookie attached — CORS
never enters into it. What constrains that page is the CSP (caveat 5: what it
may load) and the API token (what may reach the API at all), not the frame.
Giving it a real boundary means `sandbox` without `allow-same-origin`, which
needs the `postMessage` bridge in caveat 4 first.
The iframe `src` is the explicit `/dactal/index.html`, not the bare
`/dactal/` — a directory request misses the static file and hits the SPA
fallback, which serves the main app instead.

## Caveats

1. **Client-side, in-memory — no query pushdown.** DACTAL loads the working set
   into browser memory and queries it locally; it does not translate to SQL/qmd.
   You pull a bounded slice via `/api/search`, then explore it. It does **not**
   scale to the full corpus — frame it as a power-tool over a working set, not a
   replacement for the main grid.
2. **No ingestion.** DACTAL's loaders + IndexedDB store compete with the ETL
   pipeline; we use none of it and treat DACTAL as read-only over `/api/search`.
3. **Two query languages coexist** — Datalib's Gmail-style search vs.
   DACTAL's `.`/`:`/`/`/`#`. A learning curve; scoped as an optional view.
4. **Drill-down stays inside DACTAL** — clicking a row re-runs a DACTAL query, it
   does not open a Datalib document card. Wiring "open the chat" needs a
   `postMessage` bridge from the iframe to `ctx.host.openCard(...)` (not yet done).
5. **The vendored snapshot phones home — and is pinned shut by a CSP.**
   Three functions in `vendor/dactal_utils.js` load code from `dactal.org`
   at *runtime*: `dactal_ai_init()` (:325) and `loadscript()` (:381) inject
   `<script src="https://dactal.org/…">`, and `loadscript_namespaced()`
   (:393) fetches from there and runs the text through `new Function`.

   Nothing in Datalib calls them — they are **dormant, not active**
   (`dactal_ai_init` is defined and never invoked, and we never write the
   `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` localStorage keys it gates on;
   the two `loadscript` variants are reached only from inside
   `dactal_utils.js`, on dataset-declared connectors, which our host page
   does not use). But they are one call away, and a future dataset — or a
   refresh of the snapshot — could wake them. Fetching at runtime also
   quietly defeats the point of pinning: you would get whatever
   dactal.org serves *today*, with no SRI and no version pin.

   So `public/dactal/index.html` carries a CSP — `script-src 'self'
   'unsafe-eval'; connect-src 'self'` — which makes all three fail closed
   permanently. It lives in the host page rather than in `vendor/`, which
   keeps the "unmodified pinned copies" property `vendor/PROVENANCE.md`
   depends on. `'unsafe-eval'` has to stay: the query language evaluates
   expressions through `eval` (`dactal.js:2052`) and `new Function`. The
   page's own script is in `main.js`, not inline, precisely so that
   `script-src` need not allow `'unsafe-inline'` — **do not move it back
   inline.** Issue #138, mitigation 4.

6. **Licensing.** Single-author project, static JS from dactal.org; no
   license is stated in the files or on the site. Settled for our
   purposes — DACTAL is Imbue's — so this is a provenance-tracking
   question, not a trust-the-author one. See `vendor/PROVENANCE.md`.

### What it adds
Grouping, annotators (count/total/average/median…), heatmaps, and tag-clouds over
arbitrary facets — analytical views AG-Grid doesn't offer — as terse, composable,
shareable query strings.
