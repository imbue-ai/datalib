# Design: a data-centric UI

**Status: §1, §2, §4 and §6's first half built (2026-09-15); §3's
identities on the wire; §5 and `GridCard` not.** Written
2026-09-09 against `a4752fb5`; revised 2026-09-15 against `9a45cff4`.
Per [`AGENTS.md`](../../../AGENTS.md), don't cite this file as a
description of the tree. Where it says "today", that was checked
against the later commit; where it says "would", nothing exists.

**What the revision changed.** Two of the first draft's pieces landed
by other routes — the crate split
([`provider_crate_split.md`](completed/provider_crate_split.md)) and
the pipeline store plus run logs
([`logs_and_metrics.md`](completed/logs_and_metrics.md), which also
rejected this draft's per-step log files; its "One file, not one per
step" says why). The `ColumnSpec` this draft extended no longer exists.
And the Manage screen grew from a flat grid into a tree with a
commit-history grid, a log panel and in-place config editing, which
moved the lever: the expensive part is no longer the column types, it
is the join the browser does to build each row. The pieces below are
reordered around that.

## The idea

Everything datalib holds is data, and nearly all of it is either a
**table** or a **markdown document**. So the UI needs two viewers, not
twelve screens. What makes that sufficient rather than impoverished is
three things:

1. **Data is typed.** A column that holds bytes renders as a human
   size with the exact figure on hover; a timeseries renders as a
   sparkline; an identifier — a source type, a step id, a UUID —
   resolves to an icon or a name before it is shown.
2. **Data changes, and says so.** A step finishing publishes "this
   changed"; any card reading it refetches. The manager screen is not
   special: it is a table that happens to move often.
3. **Some rows do things.** A row can carry buttons that cause
   something to happen in the backend or in the UI.

The concrete test of the idea: **the Manage screen's sources tree
becomes an ordinary card in the card surface** — rows from one
endpoint, drawn by type, refetched when the backend says so. If that
works, the idea is real. If it needs a dozen escape hatches, it isn't.

## What already exists

This is not greenfield, and reading it as greenfield is the main way
to get the scope wrong.

- **The card system.** A card is a JS expression evaluated into its own
  shadow root, hosted by a layout that owns placement and chrome. Three
  layouts, a cross-card bus, host commands, a new-card gallery. See
  [`cards.md`](../cards.md) and `ui/src/cards/types.ts`.
- **Applets.** Any program can contribute card components *and* the
  endpoints behind them, declared in `config.toml`. `unified_index` is
  the precedent for an endpoints-only applet. See
  [`applets.md`](../applets.md).
- **A live channel.** One SSE connection for the whole page, carrying
  payload-free `root` frames (`config_changed`, `dag_changed`,
  `frontend_changed`, `index_changed`) that mean "ask again".
  `ui/src/live.ts` explains why there is exactly one connection.
- **A queryable run store.** `system/runs.sqlite` holds every run's
  step states, log lines and metrics, written by the runner alone, and
  `GET /api/dag` already joins it with `dag_state.json` server-side —
  each step arrives with its `last_run`, `current_state` and
  `progress`. `GET /api/runs/{run}/log` serves a run's lines as rows.
  This is what `logs_and_metrics.md` built.
- **Typed cells — hardcoded, twice.** `Manager2View.vue` has the byte
  column with a sparkline and an exact breakdown on hover, a source
  type resolved to a brand mark, a status resolved to a glyph, a
  timestamp shown as "7 days ago" with the exact stamp on hover, and a
  row of action buttons. `GridCard.ce.vue` independently has a *second*
  `formatBytes` and a *second* provider-icon renderer. The type system
  described here largely exists already; it is just written twice, in
  TypeScript, chosen by comparing field names.

## What is missing

**Each Manage row *was* assembled in the browser from six endpoints**
— `/api/config`, `/api/dag`, `/api/sync/jobs/all`,
`/api/pipeline/storage`, `/api/frontend` and `/api/runs` — with the
status rules in two TypeScript modules and a 3376-line view around
them. A card cannot be handed six endpoints and a rulebook, so that
was the first thing to fix, and §1 is it. What is still in the browser
after it is the catalog (`ui/src/config/catalog.ts`): what a source
type is called and which icon it gets.

**The wire carries no types.** There is no column schema on any
endpoint at all: `GridCard`'s `columnDefs` are hardcoded in the Vue
file, and every renderer is picked by a `field === "byte_size"`
comparison. (The first draft extended a `ColumnSpec`
`{field, header, default_visible}`; that type is gone, and the only
`ColumnSpec` in the tree now is the Miller-view layout slot in
`ui/src/router/columns.ts`, which is unrelated.)

## The pieces

### 1. The join moves server-side — built

`GET /api/manage/rows` serves the Manage rows assembled
(`datalib/backend/http/src/manage/`). It lives in `datalib-http`, not
in an applet, because every input is something `datalib-http` already
reads — the config, `dag_state.json`, the run store, the job store,
the usage store, the applet supervisor — and none of it is the grid
index, which is the one thing the applet opens and the one thing this
join never needs.

The rules are the ones `pipelineStatus.ts` and `groupRows.ts` held,
ported to Rust with their tests (`manage/status.rs`, `manage/group.rs`);
the assembly is `Manager2View`'s old `entryRow`/`groupRow`
(`manage/mod.rs`). The aggregation table in
[`groups_and_functions.md`](groups_and_functions.md) is the spec for
the group row. A row carries the entry's id and kind, its `path` in
the tree, its group, its name, its type, status with reason, last
synced, bytes with the measured series, what a sync of it starts at,
and why each action is or isn't available. The tree is a `path` on
every row, not a column type.

What stayed in the browser is what needs the wizard's catalog, which
is where the wizard is: the type's label and icon, whether the form
can edit a row, what Browse opens, and an ingest step's
"Download"/"Import" label (which reads the step's `params` against
the provider's declared methods — so a step row carries `params`).
`Manager2View.decorate` adds those per row. §3 is what would move the
first three of them server-side.

It was worth doing on its own: `Manager2View` went from 3376 lines to
2974 and lost four endpoints, and the `manager2-*` e2e specs passed
unchanged apart from a column id.

### 2. A column-type vocabulary

Column types are **declared by the producer** and travel on the wire.
The rows endpoint from §1 is the first to carry a schema:

```json
"columns": [
  { "field": "bytes",       "header": "On disk",     "type": "bytes" },
  { "field": "last_synced", "header": "Last synced", "type": "timestamp" },
  { "field": "type",        "header": "Type",        "type": "identity" }
]
```

The starting vocabulary is what the two existing grids already draw,
and nothing more — every member has a working implementation to port:

| type | rendering | exists today as |
| --- | --- | --- |
| `text` | plain | the default |
| `bytes` | human size, exact figure on hover | `Manager2View.formatBytes` + `GridCard.formatBytes` |
| `count` | grouped digits | `GridCard`'s `item_count` |
| `timestamp` | relative, exact stamp on hover, sorts on the instant | `config/timeFormat.ts` + `compareStamps` |
| `timeseries` | sparkline, calibrated across the column | `config/sparkline.ts` |
| `identity` | label, with the icon the producer named; id on hover | `config/icons.ts` + `Manager2View`'s name and type cells |
| `status` | glyph, reason on hover | `config/glyphs.ts` |
| `markdown_uuid` | title, opens the document card on click | `GridCard`'s row click |
| `actions` | buttons (see §4) | `Manager2View`'s actions cell |

Add a member when a second surface needs it, not in anticipation.

**Where the vocabulary lives.** In `datalib_schema`'s sibling position
for the app — a small crate with no datalib dependencies (the
`//datalib/backend/runtime` pattern), depended on by `datalib-http`
and by any applet that serves a table. Providers never see it, so a
rendering-vocabulary change rebuilds nothing that downloads — the
crate split's rule, applied one level up.

It is mirrored by hand as a TypeScript string union in
`ui/src/api.ts`, the way `DagRunState` and `SyncJobState` already are.
There is no generator, so the two halves change together — the repo
convention is AGENTS.md's "Name a closed set of strings", including
the rule that parsing an unknown value returns `None` rather than
guessing. A viewer meeting a type it does not know renders the raw
value; it does not render nothing.

### 3. Identifiers: the producer resolves, the viewer presents

Splitting it by *who has the knowledge* rather than by sync/async:

- **The producer resolves identity.** It ships `{id, label, icon}` —
  the group id and its display name, the source type and the
  catalog's name for it, the UUID and the document's title. It is the
  only party that can, and for a large namespace (every document in a
  mirror) a client-side table is not an option anyway.
- **The viewer owns presentation.** Given the resolved label it decides
  the icon asset, the link target, the copy-id affordance, the hover
  text. The producer names an icon as a token (`"slack"`); the viewer
  maps the token to a bundled asset, falling back to a server-served
  one for a token the bundle doesn't have.

So a source type on the wire is `{id: "slack", label: "Slack", icon:
"slack"}`, and `ui/src/config/icons.ts` keeps doing exactly what it
does today — it just stops being the thing that knows what a source
*is*. The catalog's names and icons move to the server with the join
in §1; its wizard descriptors (fields, pickers, probes) stay in the
browser, because the wizard does.

### 4. Actions, split by what they touch

Two kinds, because they have different trust stories:

- **Backend effects** — sync, cancel, remove — are **producer-declared
  by id**. A row carries `{id: "sync", label, icon, enabled,
  disabled_reason}`; the viewer maps a known id to code it already
  holds. Data decides *whether* the button appears and *what it says*;
  code decides what it does. An unrecognized id renders nothing.
  Deliberately not shipping `{method, url, body}`: a URL arriving as
  data is a capability, and an applet's rows would then be able to aim
  the browser at any same-origin endpoint.
- **UI effects** — open a document card, open the edit wizard, open
  the log panel — are **card-declared**, wired to `host.openCards` and
  the existing panels. This is where the trust boundary already is:
  card source is `new Function`'d.

That split is also the one that exists today, made explicit:
`Manager2View`'s run/stop buttons call REST endpoints, while its edit
button opens a Vue modal. The `runBlocked` / `editBlocked` /
`browseBlocked` reasons the view computes per row are the
`disabled_reason` strings, served.

### 5. Publishing changes, per dataset

`root` frames grow a table-scoped kind, still payload-free:

```json
{ "kind": "table_changed", "table": "manage.rows" }
```

A card subscribes to the tables it reads and refetches those. This
keeps the discipline `live.ts` is built on — the event says only "ask
again", because every consumer already diffs what it fetches — while
letting a card ignore a change that isn't its. Until it exists, the
sources card refetches on `config_changed` and `dag_changed`, which is
what `Manager2View` does now.

### 6. One typed table viewer

`tableView({url})` fetches rows plus their column schema and renders
them by type, as a tree when the response says so. Both existing grids
port onto it:

- **The sources tree first**, because it is the richer case: it
  exercises bytes, timeseries, status, timestamp, identity and actions
  in one screen, and it is a tree. It becomes a card,
  `sourcesView()` or similar.
- **`GridCard` second**, once the vocabulary has survived contact.

The duplicated `formatBytes` and provider-icon code are deleted rather
than shared, because after the port there is one grid.

The search bar is already shared, and it is the shape the viewer's
filtering should keep: one grammar (`datalib_query` — `key:value`, `-`
to negate, quotes, free text), a `q=` parameter on whatever serves the
rows, and the server owning what each key means. The unified grid
(`/applet/unified_index/search`) and the run log (`/api/log`) both read
it today, and the right-click "Keep only" / "Exclude all" entries are
one helper (`ui/src/grid/query.ts`) appending a token to it. A viewer
that serves its own rows joins by accepting `q=`.

AG Grid stays. It is carrying sort, filter, column state, tree data,
virtualization and resize, and replacing all of that is a different
project from declaring column types.

## Scope

**In:** the server-side join; the type vocabulary and its two
implementations; per-table change events; the typed viewer; the
sources tree ported onto it as a card; the config editor as a card;
the whole-root storage bar as app chrome; a help affordance in the
card contract.

**Out:**

- **The panels stay panels.** The sources card is not one grid: it is
  the tree plus a commit-history grid, a run-log panel, and the wizard.
  Only the tree is drawn by the viewer. The other three are opened by a
  row action (§4) and teleported out of the card; the history grid may
  well become a second typed table later, but not as part of this.
  Counting them against the checkpoint would be counting things that
  were never tabular.
- **The tabs stay.** `/sources2` still answers; it is now the card
  stack `[sourcesView(), configView()]`, and the "Manager2" tab links
  there. The card surface did not swallow the app; the Manage page
  dissolved into it.
- **Editing stays as it is** — the wizard modal, the rename-in-place on
  a group row, and the raw `config.toml` textarea, now its own card.
  How editing *should* feel is a real question and a separate one.
- **`SourcesView` is not deleted here.** It is still routed at
  `/sources` and is the escape hatch while this is proven out, the
  same way Manager2 was built alongside it.
- **The non-tabular cards keep their shapes** — `sourceDagView` (an
  SVG graph), `dactalView` (an iframe), `perseusView` (a control
  panel), `galleryView`. "Two viewers" is a claim about what most data
  needs, not a rule that everything must be a table. A card that is
  genuinely a graph should be a graph.

## Sequencing

Each step is independently useful, which matters because the later
ones are the speculative ones.

1. **The crate split.** Done.
2. **The run store.** Done, by `logs_and_metrics.md`, in a different
   shape from this draft's: one file per data root, written by the
   runner, and `GET /api/dag` already reads it.
3. **The join** (§1). Done. `Manager2View` reads the endpoint from
   its own route; `pipelineStatus.ts` and `groupRows.ts` are gone. The
   catalog's naming (§3) and the actions' ids (§4) are not on the wire
   yet — the row carries the reasons, the browser still maps them to
   buttons.
4. **The vocabulary and the viewer** (§2, §6). Done: `datalib_columns`,
   `cards/TableGrid.ce.vue`, `tableView({ url })`.
5. **The sources tree as a card.** Done: `sourcesView()` and
   `configView()`; `Manager2View.vue` is gone; the history grid, log
   panel and wizard are row-action-opened panels; the whole-root
   storage bar is app chrome (`RootStorageBar.vue`); `ctx.setHelp` is
   in the card contract and both cards use it.
6. **`GridCard` onto the viewer.** The duplication goes.

**The checkpoint, read at (5).** The sources tree became a card with
these escape hatches, each named honestly:

- `TableGrid` takes `extraColumns` (AG Grid column definitions the
  card adds beside the declared ones) and a `contextMenu`. Neither is
  used by the sources card — the Activity column that was expected to
  need the first became a `chips` type instead — but `GridCard` will
  want both.
- The sources card still decorates each row client-side with what
  needs the wizard's descriptors: `editBlocked`, the Browse card
  source, and the Download/Import label read off `params`. That is the
  seam §3 describes, not a hole in the vocabulary.
- The Type column shows icon *and* label where the old grid showed the
  icon alone; a generic `identity` cell has no way to know a column is
  narrow on purpose, and the label was judged worth its width.

That is few enough that the idea holds: (6) is mopping up.
