# Design: a data-centric UI

**Status: proposal, nothing built.** Written 2026-09-09 against
`a4752fb5`. Per [`AGENTS.md`](../../../AGENTS.md), don't cite this file
as a description of the tree. Where it says "today", that was checked
against that commit; where it says "would", nothing exists.

**Depends on** [`provider_crate_split.md`](completed/provider_crate_split.md),
**which has now landed** — so the column-type vocabulary this design
introduces no longer sits upstream of every downloader.

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

The concrete test of the idea: **the Manage screen becomes an ordinary
card in the card surface** — a tabular view of some data from a
database that updates frequently. If that works, the idea is real. If
it needs a dozen escape hatches, it isn't.

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
  `frontend_changed`) that mean "ask again". `ui/src/live.ts` explains
  why there is exactly one connection.
- **Typed cells — hardcoded, twice.** `Manager2View.vue` already has
  the byte column with a sparkline and an exact breakdown on hover, a
  source type resolved to a brand mark, a status resolved to a glyph, a
  timestamp shown as "7 days ago" with the exact stamp on hover, and a
  row of action buttons. `GridCard.ce.vue` independently has a *second*
  `formatBytes` and a *second* provider-icon renderer. The type system
  described here largely exists already; it is just written twice, in
  TypeScript, chosen by comparing field names.

## What is missing

**The wire carries no types.** `ColumnSpec` is `{field, header,
default_visible}`. Every renderer is picked by a `field === "byte_size"`
comparison in a Vue file.

**Manager2 is not a view of a table.** It joins five endpoints in the
browser — `/api/config`, `/api/dag`, `/api/sync/jobs/all`,
`/api/pipeline/storage`, `/api/frontend` — and derives each row's
status in `ui/src/config/pipelineStatus.ts`. Most of its 2331 lines are
that join, not the grid. "It's just a table" is the goal, not the
current fact.

**Pipeline state is a JSON file.** `system/dag_state.json` is not
queryable, so nothing downstream can ask it a question.

## The pieces

### 1. A column-type vocabulary

Column types are **declared by the producer** and travel on the wire.
`ColumnSpec` grows a `type`:

```json
{ "field": "bytes", "header": "On disk", "type": "bytes" }
{ "field": "last_synced", "header": "Last synced", "type": "timestamp" }
{ "field": "source_type", "header": "Type", "type": "source_type" }
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
| `source_type` | brand icon, name on hover | `config/icons.ts` |
| `status` | glyph, reason on hover | `config/glyphs.ts` |
| `step_id` | the step's display name, id on hover | `Manager2View`'s name cell |
| `markdown_uuid` | title, opens the document card on click | `GridCard`'s row click |
| `actions` | buttons (see §5) | `Manager2View`'s actions cell |

Add a member when a second surface needs it, not in anticipation.

**Where the vocabulary lives.** A new leaf crate with no datalib
dependencies (the `//datalib/backend/runtime` pattern), depended on
only by applets — which are the things that serve tables. Providers
never see it. That keeps a rendering-vocabulary change from rebuilding
anything that downloads, which is the same motivation as the crate
split this design depends on, applied one level up.

It is mirrored by hand as a TypeScript string union in
`ui/src/api.ts`, the way `DagRunState` and `SyncJobState` already are.
There is no generator, so the two halves change together — the repo
convention for this is in AGENTS.md's "Name a closed set of strings",
including the rule that parsing an unknown value returns `None` rather
than guessing. A viewer meeting a type it does not know renders the raw
value; it does not render nothing.

### 2. Identifiers: the producer resolves, the viewer presents

Splitting it by *who has the knowledge* rather than by sync/async:

- **The producer resolves identity.** It ships `{id, label}` — the step
  id and the step's display name, the UUID and the document's title.
  It is the only party that can, and for a large namespace (every
  document in a mirror) a client-side table is not an option anyway.
- **The viewer owns presentation.** Given the resolved label it decides
  the icon asset, the link target, the copy-id affordance, the hover
  text. The producer names an icon as a token (`"slack"`); the viewer
  maps the token to a bundled asset, falling back to a server-served
  one for a token the bundle doesn't have.

So `source_type` on the wire is `{id: "slack_api", label: "Slack",
icon: "slack"}`, and `ui/src/config/icons.ts` keeps doing exactly what
it does today — it just stops being the thing that knows what a source
*is*.

### 3. Pipeline state as a table

> **Superseded** by [`logs_and_metrics.md`](logs_and_metrics.md), which
> built the run history and log tables as one file per data root
> (`system/runs.sqlite`) rather than one per step. The incrementality
> ledger (`dag_state.json`'s `steps`) stays a JSON file. §3 and §4 below
> are kept as the argument that was made; the decision is in that plan.

`system/dag_state.json` becomes `system/pipeline.sqlite`.

**Plain SQLite**, opened through doltlite's `doltlite_engine=sqlite`
URI parameter, WAL, following `datalib/backend/progress/src/bus.rs`
(and `etl/src/fingerprint_cache.rs`, which made the same call for the
same reasons). Three arguments, and one of them is about being able to
change our mind:

- **Consistent reads are free.** A reader in WAL sees a consistent
  snapshot as of the start of its read transaction and is never
  blocked by the writer. Doltlite does not give this for free: a plain
  `SELECT` reads the *working set*, not HEAD, so a consistent view
  needs commits plus an explicit `dolt_at_<t>('<hash>')`.
- **History is a column.** `run_id` makes "what did this step do last
  Tuesday" an ordinary query, and pruning an ordinary `DELETE`.
  AGENTS.md already made this exact call for `usage.doltlite_db`: *the
  rows are the history, and a `dolt_commit` per sample would flood
  `dolt_log` with nothing the table doesn't already say.* Step runs are
  that same shape.
- **The decision is reversible.** The engine is a URI parameter and the
  same `sqlx` code drives either one, so if the run history turns out
  to want branching or diffing that a `run_id` column cannot express,
  switching is a contained change to one file's open path — not a
  rewrite of its readers.

Three tables:

```
steps      -- durable: input_versions, output_versions, fingerprint, succeeded
runs       -- run_id, started_at, finished_at
step_runs  -- run_id, step, state, done, total, msg,
           -- started_at, finished_at, attempts, error
```

`step_runs` **replaces both** `CurrentRun.plan` + `CurrentRun.states`
*and* `system/progress.sqlite`'s `step_progress`. Those are the same
fact written twice today: `ProgressRow.state` is documented as "a
`LiveState`, or the terminal status the scheduler gave it" — which is
`RunState`, exactly what `states` holds, written by the same process.
And `plan` is just "which steps have a row this run". So: **the rows
that exist are the work expected; `done`/`total` is how much of it is
finished.**

Rows accumulate rather than being truncated per run, which is what
makes a step's history queryable. That needs a retention rule; the
simplest honest one is a run count, applied by the runner at start.

What does *not* merge is the `steps` table — `input_versions`,
`output_versions`, `fingerprint`, `succeeded`. That is not "what
happened" but "is this up to date": the incrementality ledger, and the
one part that must survive a crash. Merging it into one file means
giving up the progress bus's `synchronous=Off`, which at a 200ms flush
and a measured ~0.3ms per row costs nothing worth naming.

Structurally this fits what is there: `progress` is already a leaf
crate written by `dag` and read by `http`.

### 4. Run logs, beside the data

Today a job's log is one text file at `<root>/state/job-logs/<id>.log`,
fetched **whole** over `/api/sync/jobs/{id}/log`, and then parsed as
NDJSON and filtered down to a single step *in the browser*
(`ui/src/config/stepLog.ts`). So "show me this step's log" means
downloading every step's log and discarding most of it.

Logs become rows, in a store beside the data the step writes:

```
<step_id>/run_logs.sqlite     step_run_logs(run_id, ts, level, text, fields)
system/run_logs.sqlite        the runner's own lines, same schema
```

Per-step rather than central, because per-step is the query people
actually make — and because the logs then travel with the data:
delete a source's directory and its history goes with it.
"Everything that happened in run X" becomes a fan-out across the
participating steps, which `step_runs` can name. Since **a step's `id`
is its single output tree** (see `step_identity.md`, which shipped),
there is no ambiguity about which directory that is.

**Who writes it is neither the step author nor only the runner.** The
step already *emits* structured logs: every built-in calls
`datalib_obs::init`, which sets up `tracing` — pretty on a TTY, JSON
off it. So the sink belongs in `datalib_obs`: one more `tracing` layer
writing to `<step_id>/run_logs.sqlite`, self-configuring from
`DATALIB_DAG_STEP` and `DATALIB_DAG_DATA_ROOT`, which the step protocol
already sets.

That gives the property that motivated this: **running a step by hand,
outside the DAG runner, produces its logs too** — and no step author
writes any logging code. A third-party step that does not link
`datalib_obs` writes plain stderr, which the runner already captures
and can write to the same store on the step's behalf. Coverage is
complete either way.

### 5. Actions, split by what they touch

Two kinds, because they have different trust stories:

- **Backend effects** — sync, cancel, remove — are **producer-declared
  by id**. A row carries `{id: "sync", label, icon, enabled,
  disabled_reason}`; the viewer maps a known id to code it already
  holds. Data decides *whether* the button appears and *what it says*;
  code decides what it does. An unrecognized id renders nothing.
  Deliberately not shipping `{method, url, body}`: a URL arriving as
  data is a capability, and an applet's rows would then be able to aim
  the browser at any same-origin endpoint.
- **UI effects** — open a document card, open the edit wizard — are
  **card-declared**, wired to `host.openCards` and the existing modals.
  This is where the trust boundary already is: card source is
  `new Function`'d.

That split is also the one that exists today, made explicit:
`Manager2View`'s run/stop buttons call REST endpoints, while its edit
button opens a Vue modal.

**The wizard stays a modal**, opened by a row action, exactly as now.
Its multi-step credential-and-probe flow is genuinely not tabular, and
pretending otherwise would cost more than it buys. See
[`source_wizard.md`](source_wizard.md).

### 6. Publishing changes, per dataset

`root` frames grow a table-scoped kind, still payload-free:

```json
{ "kind": "table_changed", "table": "pipeline.steps" }
```

A card subscribes to the tables it reads and refetches those. This
keeps the discipline `live.ts` is built on — the event says only "ask
again", because every consumer already diffs what it fetches — while
letting a card ignore a change that isn't its.

### 7. One typed table viewer

`tableView({url})` fetches rows plus their column schema and renders
them by type. Both existing grids port onto it:

- **`Manager2View` first**, because it is the richer case: it exercises
  bytes, timeseries, status, timestamp, step_id and actions in one
  screen. It becomes a card, `sourcesTableView()` or similar.
- **`GridCard` second**, once the vocabulary has survived contact.

The duplicated `formatBytes` and provider-icon code are deleted rather
than shared, because after the port there is one grid.

AG Grid stays. It is carrying sort, filter, column state,
virtualization and resize, and replacing all of that is a different
project from declaring column types.

## Scope

**In:** the type vocabulary and its two implementations; the pipeline
store; run logs as rows; per-table change events; the typed viewer;
Manager2 ported onto it as a card.

**Out:**

- **The tabs stay.** Manage remains reachable at its own route. The
  card surface gains the sources table as one of the things it can
  show; it does not swallow the app.
- **Editing stays as it is** — the wizard modal and the raw
  `config.toml` textarea. How editing *should* feel is a real question
  and a separate one.
- **`SourcesView` is not deleted here.** It is the escape hatch while
  this is proven out, the same way Manager2 was built alongside it.
- **The non-tabular cards keep their shapes** — `sourceDagView` (an
  SVG graph), `dactalView` (an iframe), `perseusView` (a control
  panel), `galleryView`. "Two viewers" is a claim about what most data
  needs, not a rule that everything must be a table. A card that is
  genuinely a graph should be a graph.

## Sequencing

Each step is independently useful, which matters because the later
ones are the speculative ones.

1. **The crate split** —
   [`provider_crate_split.md`](completed/provider_crate_split.md). Done.
2. **`system/pipeline.sqlite`.** Replace `dag_state.json` and
   `progress.sqlite`; `GET /api/dag` reads the new store. No UI change
   yet — this is a pure substitution, verified by the existing tests.
3. **The vocabulary and the viewer.** `tableView` plus the type crate,
   with Manager2's columns as the first implementation. Manager2 keeps
   working from its own route while the card is built beside it.
4. **Manager2 as a card.** Delete the Vue view once the card matches
   it; keep the route pointing at the card surface.
5. **`GridCard` onto the viewer.** The duplication goes.
6. **Run logs as rows.** Independent of all of the above and can move
   at any point after (2); it is listed last only because nothing else
   waits on it.

The honest checkpoint is after (4). If the sources table is a card and
the vocabulary did not need a pile of one-off escape hatches to get it
there, the idea holds and (5) and (6) are mopping up. If it did, stop
and read what the escape hatches were — they are the design saying
what it got wrong.
