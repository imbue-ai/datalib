# Design: a data-centric UI

**Status: proposal, nothing built.** Written 2026-09-09 against
`a4752fb5`; revised 2026-09-15 against `9a45cff4`. Per
[`AGENTS.md`](../../../AGENTS.md), don't cite this file as a
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

**Each Manage row is assembled in the browser from six endpoints.**
`Manager2View` fetches `/api/config`, `/api/dag`, `/api/sync/jobs/all`,
`/api/pipeline/storage`, `/api/frontend` and `/api/runs`, then derives
each row — its status word and reason, what "last synced" means for a
group, which child a group's status comes from, which buttons apply
and why not — in `ui/src/config/pipelineStatus.ts` (341 lines) and
`ui/src/config/groupRows.ts` (114 lines), keyed by a client-side
catalog (`ui/src/config/catalog.ts`) that says what a source type is
called and which icon it gets. The view is 3376 lines, and most of them
are that join and the panels around it, not the grid. "It's just a
table" is the goal, not the current fact — and a card cannot be handed
six endpoints and a rulebook.

**The wire carries no types.** There is no column schema on any
endpoint at all: `GridCard`'s `columnDefs` are hardcoded in the Vue
file, and every renderer is picked by a `field === "byte_size"`
comparison. (The first draft extended a `ColumnSpec`
`{field, header, default_visible}`; that type is gone, and the only
`ColumnSpec` in the tree now is the Miller-view layout slot in
`ui/src/router/columns.ts`, which is unrelated.)

## The pieces

### 1. The join moves server-side

One endpoint serves the Manage rows, assembled: `GET /api/manage/rows`
(the name is a placeholder). It lives in `datalib-http`, not in an
applet, because every input is something `datalib-http` already reads
and serves — the config, `dag_state.json`, the run store, the job
store, the usage store, the frontend registry — and none of it is the
grid index, which is the one thing the applet opens and the one thing
this join never needs.

The rules it applies are the ones `pipelineStatus.ts` and
`groupRows.ts` hold today, ported to Rust: they are already pure
functions over the config entries and each child's status view, and
their unit tests port with them. The aggregation table in
[`groups_and_functions.md`](groups_and_functions.md) is the spec for
the group row.

A row is what `Manager2View`'s `Row` type is now, minus the fields
that exist only to drive a Vue template: the entry's id and kind, its
`path` in the tree, its group, its display name, its resolved type
(§2), status with reason, last synced, bytes with the measured series,
row count, what a sync of it starts at, and its actions (§4). Rows
form a tree, so the response says so once — `tree: true` and a `path`
on every row — rather than pretending the tree is a column type.

**This step is worth doing on its own.** `Manager2View` reads the new
endpoint from its own route, `pipelineStatus.ts`, `groupRows.ts` and
the catalog lookups go, and the existing `manager2-*` e2e specs say
whether the rows still mean the same thing. Nothing about cards has to
exist yet.

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

AG Grid stays. It is carrying sort, filter, column state, tree data,
virtualization and resize, and replacing all of that is a different
project from declaring column types.

## Scope

**In:** the server-side join; the type vocabulary and its two
implementations; per-table change events; the typed viewer; the
sources tree ported onto it as a card.

**Out:**

- **The panels stay panels.** `Manager2View` is not one grid: it is the
  sources tree plus a commit-history grid, a run-log panel, and the
  wizard. Only the tree is the card. The other three are opened by a
  row action (§4), as the wizard already is; the history grid may well
  become a second `tableView` later, but not as part of this. Counting
  them against the checkpoint would be counting things that were never
  tabular.
- **The tabs stay.** Manage remains reachable at its own route. The
  card surface gains the sources tree as one of the things it can
  show; it does not swallow the app.
- **Editing stays as it is** — the wizard modal, the rename-in-place on
  a group row, and the raw `config.toml` textarea. How editing *should*
  feel is a real question and a separate one.
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
3. **The join** (§1, with §3's identities and §4's actions as row
   fields). `Manager2View` reads the new endpoint from its own route;
   `pipelineStatus.ts`, `groupRows.ts` and the catalog's naming go.
   Verified by the existing `manager2-*` e2e specs — no new UI.
4. **The vocabulary and the viewer** (§2, §6). `tableView` plus the
   type crate, with the sources tree's columns as the first
   implementation. Manager2 keeps working from its own route while the
   card is built beside it.
5. **The sources tree as a card.** Delete the tree half of the Vue
   view once the card matches it; the history grid, log panel and
   wizard become row-action-opened panels.
6. **`GridCard` onto the viewer.** The duplication goes.

The honest checkpoint is after (5). If the sources tree is a card and
the vocabulary did not need a pile of one-off escape hatches to get it
there, the idea holds and (6) is mopping up. If it did, stop and read
what the escape hatches were — they are the design saying what it got
wrong. Step (3) pays for itself whatever the checkpoint says.
