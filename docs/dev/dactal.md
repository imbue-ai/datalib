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

`opts.load` is a Frankweiler search that seeds the working set; `opts.q` is the
initial DACTAL query. Both flow to the page as `?fw=`/`?dq=`.

It is **data-agnostic**: there is no per-provider code. The page loads whatever
`/api/search` returns and converts it to DACTAL datasets on the fly (`bridge.js`),
so it works over any corpus — Slack, GitHub, Notion, Perseus, all of it.

## What DACTAL is

A single-author, dependency-free, **client-side** data explorer distributed as
three classic (non-module) browser scripts, vendored under
`frankweiler/ui/public/dactal/vendor/` (see `vendor/PROVENANCE.md`):

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

- **App glue** (`frankweiler/ui/src/cards/`): `libs/dactalView.ts` (the factory),
  registered in `libs/index.ts`, typed in `types.ts`, and advertised in the
  empty-card hints in `components/ShadowCard.vue`.
- **Served page** (`frankweiler/ui/public/dactal/`):
  - `bridge.js` — the **only** Frankweiler-specific glue: maps each `grid_rows`
    row to a DACTAL item (`uuid`→`id`) and re-normalizes the facet columns
    (`author`, `channel`, `source`, `account`, `project`, `conversation`, …) into
    id-keyed entity datasets so DACTAL's relational joins light up on top of the
    denormalized table. It calls `dactal.survey()` after loading — required, or
    `autoresolve` never fires and `rows.author` stays a bare string.
  - `index.html` — the explorer page loaded in the iframe. Two inputs: a
    Frankweiler search (pulls a working set into the browser) and a DACTAL query
    over it. Reuses DACTAL's engine + renderer but not its host page (no
    saved-query store / AI assist / adapters).
  - `vendor/` — the three pinned DACTAL scripts.

`public/**` is already in `frankweiler/ui/BUILD.bazel`'s `vite_inputs`, so the
page ships in packaged builds with no extra wiring; the embedded server serves
`/dactal/index.html` the same as vite dev.

### Why an iframe (not a `vueCard`)
DACTAL ships as classic scripts that attach to `window` globals, assume a single
engine instance per page, and emit inline `onclick=` handlers that resolve
against the top-level window. Mounting that into a card's Shadow DOM would break
the inline handlers and cap us at one DACTAL card per app (shared globals). The
iframe gives each card its own window/engine/IndexedDB and full isolation from
the Vue app. The iframe `src` is the explicit `/dactal/index.html`, not the bare
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
3. **Two query languages coexist** — Frankweiler's Gmail-style search vs.
   DACTAL's `.`/`:`/`/`/`#`. A learning curve; scoped as an optional view.
4. **Drill-down stays inside DACTAL** — clicking a row re-runs a DACTAL query, it
   does not open a Frankweiler document card. Wiring "open the chat" needs a
   `postMessage` bridge from the iframe to `ctx.host.openCard(...)` (not yet done).
5. **Provenance & licensing.** Single-author project, static JS from dactal.org;
   **no license is stated** in the files or on the site. A pinned snapshot is
   vendored under `public/dactal/vendor/` — **resolve licensing with the author
   before shipping a distributed build.** See `vendor/PROVENANCE.md`.

### What it adds
Grouping, annotators (count/total/average/median…), heatmaps, and tag-clouds over
arbitrary facets — analytical views AG-Grid doesn't offer — as terse, composable,
shareable query strings.
