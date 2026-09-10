# datalib — agent runbook

Quick references for AI/human contributors working **on the datalib
codebase**: where the docs are, how the repo is laid out, and the
conventions that aren't obvious from the code. If you are an agent
*using* datalib (running syncs, querying a user's mirror, writing a
custom step), start with [`agent_user.md`](docs/agent_user.md) instead.

## Doc map

Start here when a task touches an area you don't already know. All paths
are relative to the repo root.

**Only the docs directly under `docs/dev/` describe the tree.** Three
subdirectories hold things that do not, and each says so in its own
banner:

| | |
|---|---|
| [`plans/`](docs/dev/plans/) | intended, not built |
| [`plans/completed/`](docs/dev/plans/completed/) | landed, kept as the record of what was decided |

A plan that lands moves to `plans/completed/`. The exception is a plan
somebody would read to *learn how the system works*: rewrite that one
as reference and put it directly under `docs/dev/`. When a completed
plan stops being worth keeping, **delete it** — git has it, and a
directory of obsolete prose is a liability here rather than an asset.
The entries below stay grouped by topic, so a plan sits beside the
reference doc it relates to.

**Pipeline / sync engine**

- [`datalib/backend/dag/README.md`](datalib/backend/dag/README.md) — the
  runner's current rules: how the graph is built and what gets dropped
  from it, what makes a step stale, why versions are reported by the step
  rather than measured by the runner, what each diagnostic severity
  costs, and the two locks.
- [`docs/dev/pipeline_dag_architecture.md`](docs/dev/pipeline_dag_architecture.md)
  — the design history behind that: why a DAG at all, the node contract
  as it was proposed, the implementation decisions and the open
  questions.
- [`docs/dev/plans/completed/step_identity.md`](docs/dev/plans/completed/step_identity.md)
  — **built (2026-08-31)**: a step's `id` *is* the one tree it writes,
  `inputs` name step ids, and `outputs` is gone from the config
  entirely. Read it for why; it was written as the design and kept as
  the explanation.
- [`docs/dev/plans/groups_and_functions.md`](docs/dev/plans/groups_and_functions.md)
  — *agreed design (2026-09-09); slices 1, 2 and 4a built (2026-09-09
  and 2026-09-10), the rest not*: one row per source in the Manage
  screen, done by making the grouping a config entity. A `[[groups]]`
  table with `id`/`name`/`type`; a step is `(group, function)` with its
  id composed and never written; `datalib-step` dispatches on the
  function and the group's type from the environment and writes the
  tree its id names, so a built-in step carries no `command`; the
  trees are named after the functions (`ingest`, `render_markdown`,
  `grid_index`, `qmd_index`) — **that much is in the tree** (the
  loader, the runner's environment, the fingerprint rule,
  `datalib-step`, every config and fixture), **and so is the row**: the
  Manage screen is a tree, one row per group with its steps and applets
  under a chevron, the group row reading status, last-synced and bytes
  off its own folder and its children (`ui/src/config/groupRows.ts`
  holds the rules). Still to come: `type` as the data type with the
  fetch method as a params table, each declared `Origin` or `Local`,
  which is what makes a row read "Download" or "Import"; the one-dialog
  wizard; the mechanical crate rename.
  Read it before touching step ids, the wizard, or `datalib-step`'s
  dispatch. It reverses the "ungrouping" section of `step_identity.md`.
- [`docs/dev/plans/streaming_steps.md`](docs/dev/plans/streaming_steps.md) —
  *proposal*, nothing built: letting a consumer step start before its
  producer finishes. Splits the two meanings an edge carries today
  ("B consumes A's output" and "B may assume A is finished"). The
  doltlite side is verified — `dolt_at_<t>('<hash>')` is the `AS OF`
  we thought we didn't have, and a plain `SELECT` reads the *working
  set*, not HEAD. Reproducer: `hack/doltlite_concurrent_reader/`.
- [`docs/dev/plans/streaming_steps_plan.md`](docs/dev/plans/streaming_steps_plan.md)
  — *plan*, partly built: how to build the above, measured against the
  tree, with each step marked done or not. Read it before touching how
  any consumer reads a store — its §"The hazard" is the one to know,
  because the cursor scans are already safe under a live writer and
  every *content* read is not. §"The sink contract" is the one to know
  before writing anything that *deletes* on an empty read: a sink that
  cannot tell "absent" from "empty" gets its whole source swept, which
  has happened here twice. It also inventories what already exists
  (more than the proposal above implies) and overturns two of that
  proposal's conclusions.
- [`datalib/backend/dag/src/diagnostics.rs`](datalib/backend/dag/src/diagnostics.rs)
  — **read before changing how a config is validated**: why the loader
  returns a list of diagnostics rather than an `Err`, and what
  separates the four severities (blast radius — how much of the file
  one problem costs). The rules themselves sit beside them in
  `config.rs::accept_steps` and `graph.rs::build_graded`.
- [`docs/dev/step_protocol.md`](docs/dev/step_protocol.md) — **how to
  write a custom step command**: the config entry, the `--params` /
  `--inputs` / `--outputs` flags, `DATALIB_DAG_*` env vars, the
  NDJSON progress/outcome protocol, failure classification, and
  cancellation. Any executable can be a step; `datalib-step` is the
  reference implementation.
- [`docs/dev/plans/data_lib_as_a_library/`](docs/dev/plans/data_lib_as_a_library/)
  — two linked *proposals* (nothing built) about datalib as something
  others build on, prompted by the `data-pipeline-builder` skill in
  `imbue-ai/default-workspace-template#534`, plus
  [`render_audit_2026_09_03.md`](docs/dev/plans/data_lib_as_a_library/render_audit_2026_09_03.md)
  — the first of those proposals' audit actually run, and the one file
  here that is measurement rather than intent (read it before believing
  any claim about what render does today).
  [`data_handling_practices.md`](docs/dev/plans/data_lib_as_a_library/data_handling_practices.md)
  is the one to read first and the one that touches this repo: the
  seven things that skill does better than we do, the four audit
  passes over the providers we already shipped, and what a new
  provider has to do from now on. Its §1 scorecard is the honest
  version — we are behind on **everything about the record we cannot
  store**.
  [`toolchain_for_agents.md`](docs/dev/plans/data_lib_as_a_library/toolchain_for_agents.md)
  is downstream of it; its §1 inventories what the five file-backed
  providers already share (`fswalk`, `file_checkpoint`, `input_path`,
  the content-vs-path identity split) — read that before concluding
  datalib only mirrors web APIs, and its §2 before claiming it can
  ingest arbitrary records, which it can't yet.
- [`configs/dag_example.toml`](configs/dag_example.toml) — a complete,
  commented steps-format config, including the recipe for running
  `datalib-dag` from a bazel build.

**Data architecture**

- [`datalib/backend/etl/README.md`](datalib/backend/etl/README.md) — the
  rules for the shared ingest machinery: raw-store primary keys, the
  bookkeeping sidecar, volatile fields, JSONB payloads, why every doltlite
  pool is size 1, and why the DDL runs in two passes.
- [`datalib/backend/etl/macros/README.md`](datalib/backend/etl/macros/README.md)
  — the four table derives (`WirePayloadRow`, `RawTable`, `CasEdgeRow`,
  `PortableTable`): required struct shape, attributes, and the Rust→SQL
  type mapping.
- [`docs/dev/data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md)
  — the download (ingestion) architecture: raw stores, incrementality,
  resumability, wire tape. Companion:
  [`data_architecture_ingestion_practices.md`](docs/dev/data_architecture_ingestion_practices.md)
  (how to build a new provider). The two split along a
  principles/practitioner line in `dab2c3d9`; both are scoped to
  **download**.
- [`docs/dev/data_architecture_parse_and_render.md`](docs/dev/data_architecture_parse_and_render.md)
  — the **parse and render** stage, the third sibling: deserializing a
  stored payload, projecting it to `GridRow` + markdown, the
  data-quality rules (§4 — adopted in principle, *not implemented*),
  incrementality, and the `GridRow.when_ts` policy. Read it before
  adding a renderer or changing a projection. There is no "parse
  step": a record that "fails to parse" is one **render** could not
  deserialize, and the fix is always a re-render, never a re-fetch.
- [`datalib/backend/etl/providers/media/DOWNLOAD.md`](datalib/backend/etl/providers/media/DOWNLOAD.md)
  — the `media` source: local music/photos/video/playlists. Read it
  before touching anything about **`payload_blake3`**, the
  metadata-excluding second hash (per-container recipes, why an
  unparsable container gets NULL rather than the file hash, why the
  scheme name is stored beside the digest). Also covers the
  audio-vs-visual table split, why playlists keep their unresolvable
  entries, and the one place this repo's timestamp convention is
  deliberately deviated from.
- [`docs/dev/email_download_modes.md`](docs/dev/email_download_modes.md)
  — the `email` source's three download modes (JMAP, Gmail API, mbox),
  what keeps them writing one deduped schema, and why an IMAP mode was
  built and removed.
- [`docs/dev/grid_rows.md`](docs/dev/grid_rows.md) — the `grid_rows`
  union table behind the grid UI. Its per-provider mapping tables name
  raw-store tables and columns; check those against the
  `schema_inventory` golden
  (`datalib/backend/schema_inventory/`), which is generated from the
  DDL and so is the one list that cannot be stale. Prose here has been
  wrong before — it named `openai_conversations`, `claude_conversations`
  and `slack_workspaces`, none of which have ever existed.
  Its last two sections cover the **storage rows** every source emits
  (what a mirror weighs, and the row counts inside it) — read those
  before changing `datalib_step/src/introspect.rs`, and in particular
  before moving the measurement *history* into `grid_rows`, which was
  considered and rejected for four reasons written down there.
- [`docs/dev/edges.md`](docs/dev/edges.md) — the cross-document `edges`
  table.
- [`docs/dev/entity_ids.md`](docs/dev/entity_ids.md) — **read before
  adding a provider or touching any `*_uuid` recipe**: the one rule for
  minting `grid_rows.uuid`, why the scope is never our `source_name`
  (nor `source_type`), the `source_native_id` backpointer, and the
  per-provider porting status.
- [`docs/dev/doltlite.md`](docs/dev/doltlite.md) — inspecting
  `.doltlite_db` files (CLI, `dolt_*` vtabs, rescue commits); tutorial in
  [`doltlite_codelab.md`](docs/dev/doltlite_codelab.md).
- [`docs/dev/provider_migration_dolt_diff_and_cas_edge.md`](docs/dev/provider_migration_dolt_diff_and_cas_edge.md)
  — the live recipe for porting the remaining providers to CAS blobs +
  incremental render.
- [`docs/dev/plans/multimodal_retrieval.md`](docs/dev/plans/multimodal_retrieval.md)
  — *proposal*, nothing built: replacing the `qmd_index` step with a
  retrieval layer that takes an arbitrary `grid_rows` metadata
  prefilter and holds more than one vector space. Read §4 ("bytes at
  rest") before touching how text is stored anywhere — it measures a
  real data root and finds the same text kept **five** times (raw,
  rendered `.md`, `grid_rows.text`, and *twice* inside qmd, whose FTS5
  is declared without `content=`), attachment bytes kept twice, and
  nothing compressed at rest.

**UI**

- [`docs/dev/cards.md`](docs/dev/cards.md) — the card system (custom
  views, component library); [`docs/dev/dactal.md`](docs/dev/dactal.md)
  — the dactal view bridge.
- [`datalib/backend/etl/chat-common/README.md`](datalib/backend/etl/chat-common/README.md)
  — **read before changing how a chat message looks**: the one markdown
  layout all ten chat providers render through. Why the message
  header has to stay an `h2` (qmd cuts its chunks there), how a run of
  tool calls folds into one collapsed `<details>`, and `LAYOUT_VERSION`
  — the one number to bump so all eight re-render. It also points at
  `bazelisk run //datalib/ui:render_preview`, which rewrites
  `datalib/ui/tests/goldens/render_preview.html`: **every** provider's
  rendered markdown — the TNG fixture's real output, plus chat-common's
  synthetic corpus — drawn through the app's own markdown-it, card CSS
  and decoration module, so a rendering change is reviewed by opening a
  file rather than by building a data root.
- [`docs/dev/plans/data_centric_ui.md`](docs/dev/plans/data_centric_ui.md) —
  *proposal*, nothing built: one typed table viewer plus the markdown
  one, with column types declared by whoever serves the rows, and the
  Manage screen ported onto it as an ordinary card. The crate split it
  depended on has landed.
- [`docs/dev/wizard_file_pickers.md`](docs/dev/wizard_file_pickers.md)
  — **read before adding a source to the Add/Edit wizard**: a field
  that asks for a file or folder must offer a native OS picker, not a
  text box. How the three layers fit (Tauri capability → `pickPath` →
  the button), the checklist for a new path field, and why the
  browser-served case can't have one. The wizard's own design is
  [`docs/dev/plans/source_wizard.md`](docs/dev/plans/source_wizard.md) (a
  proposal, only partly built — read its banner); the descriptors you
  actually edit are `datalib/ui/src/config/catalog.ts`.
- [`docs/dev/plans/qmd_index_ui.md`](docs/dev/plans/qmd_index_ui.md) — the grid's
  `Indexed` / `Embedded` columns and the `qmd_state` endpoint behind
  them (built), plus the design for selective re-indexing and live
  index progress (proposal — the file marks which is which).
- [`docs/dev/applets.md`](docs/dev/applets.md) — **how to write an
  applet**: the second kind of config entry, a server contributing card
  components plus the endpoints behind them. Covers the
  one-invocation contract (`-p 0` + `--frontend-dir`: write, bind,
  then announce the port on stdout),
  the `system/frontend/<namespace>/` store that any program (or person)
  can write into, and why two instances of one command share a
  component but not its arguments.

**Dev workflow**

- [`docs/dev/first_time_dev.md`](docs/dev/first_time_dev.md) — build and
  run from source.
- [`docs/dev/testing.md`](docs/dev/testing.md) — the test suites;
  [`docs/dev/coverage.md`](docs/dev/coverage.md) — coverage runs.
- [`docs/dev/docker.md`](docs/dev/docker.md) — the container image.
- [`docs/dev/plans/completed/provider_crate_split.md`](docs/dev/plans/completed/provider_crate_split.md)
  — **built**: download and render are separate crates, so a
  render-schema change no longer rebuilds every downloader (105 test
  targets downstream of `datalib_schema`, now 79). Read it for the
  measurements and for the three things the proposal got wrong; the
  rules it leaves behind are in §"Download and render are separate
  crates" below.

**User-facing**

- [`docs/user/first_time_user.md`](docs/user/first_time_user.md),
  [`docs/user/getting_your_data.md`](docs/user/getting_your_data.md),
  and [`docs/user/config_examples/`](docs/user/config_examples/) (one
  commented group with its `ingest` + `render_markdown` step pair per
  source).

## Prose can be stale — verify claims against the tree

The docs above, `TODO.md`, and this repo's commit messages are unusually
detailed and well-argued. That is exactly what makes a wrong one
dangerous: a well-reasoned paragraph reads as evidence, so an incorrect
claim tends to get repeated rather than checked.

**Before reporting any "we now do X" or "X still needs doing" claim as
current fact, verify it against the tree or the diff.** The checks are
cheap:

```sh
git show --stat <sha>                    # did that commit touch what its message says?
git log --diff-filter=A -- <path>        # was this file ever actually added?
grep -rn <thing-said-to-exist> <subtree> # is the thing there at all?
```

Two confirmed instances, both found 2026-08-17:

- `TODO.md` led with "expunge the manual-e2e test data from git HISTORY"
  as a pending pre-open-sourcing blocker. The purge had already been done.
  `git filter-repo` preserves commit messages and the working tree, so the
  instruction outlived its own completion — and `docs/dev/testing.md`
  carried a second copy citing `TODO.md` as its source (#112, #120).
- `b27039d0` states it gave a toothless slack test teeth with a "poison
  fixture". Its diff touches 20 files, none of them the test file, and the
  comment the message itself calls out as false is still there verbatim
  (#123).

**Test-quality claims are the highest-risk category**, because a false one
is self-concealing: if a test cannot fail, nothing downstream will ever
reveal that the claim was wrong. Treat "now covered by a test" as
unverified until you have read the assertion — and for a test whose job is
to catch a silent no-op, until you have watched it fail against the broken
behavior.

When prose and the tree disagree, the tree wins. Fix the prose in the same
change.

## Write plainspoken

In docs — and in the few comments you keep — be clear and unhurried,
explain a term the first time it appears, and don't assume the reader
already shares your context. A lot of the prose already here is terser and
more jargony than it should be, so the surrounding text is not the register
to match. Plainspoken means *clear*, not *long*: see the next section for
how little of it belongs in the code itself.

## Comments: few, short, and about *why*

**Write the code as if comments did not exist.** A comment is the fallback
for what you could not say in a name or a shape. Reach for a better name,
a smaller function, or a named intermediate variable first; add the comment
only when you have run out of code to say it with.

Keep these:

- **A file header.** One to three sentences or bullets: what this file is
  for, what belongs here, and — where it is not obvious — what does not.
- **A type header.** Same shape, for a struct/enum/trait/class that is not
  self-evident from its name and fields.
- **A *why* that the code cannot carry.** A non-obvious constraint, a
  workaround for someone else's bug, a trap the next person will fall into.
  Say it in a sentence or two, and prefer stating the rule over narrating
  how we arrived at it.

Delete these on sight:

- **Function-level doc blocks.** If a function needs a paragraph to explain
  what it does, rename it or split it. The name should give it all away.
- **Restatement.** `// increment the counter` above `counter += 1`.
- **Changelog.** "used to", "before #209", "this replaced the old…",
  "as of 2026-08-31 we…". Git already knows. So does the issue tracker.
  A comment that dates itself is a comment that will be wrong.
- **Essays.** Section banners, numbered arguments, transcripts of a
  decision. If it is genuinely worth several paragraphs it is documentation,
  not a comment — put it in a `README.md` beside the code (or under `docs/`)
  and, if the reader really needs the pointer, link it in one line.

Tests are the one place a short doc comment on a function earns its keep: a
sentence or two naming the regression it guards, especially where the test
would otherwise look like it asserts nothing interesting. Still a sentence or
two — not the incident report.

Rule of thumb: if you are about to write a fourth consecutive comment line,
you are writing a document. Stop and decide where it belongs.

Every comment is a claim that has to be re-verified on every edit, and an
unverified claim in this repo has already burned us more than once — see
[Prose can be stale](#prose-can-be-stale--verify-claims-against-the-tree).
Fewer, truer comments beat more of them.

## Repo layout

```
datalib/
  backend/     Rust workspace.
    dag/           `datalib-dag`: the DAG runner (scheduler, step
                   contract, subprocess driver, NDJSON event stream).
                   `//datalib/backend:bin` stages it plus every other
                   shipped binary under their public `datalib-*` names
                   in one directory (`:dist`, laid out as installed) —
                   build that, not the individual targets, whenever you
                   need to actually run a pipeline.
    datalib_step/  `datalib-step`: the built-in step program. A step
                   with no `command` runs it; it reads its function
                   (ingest, render_markdown, grid_index, qmd_index) and
                   its group's type from the environment.
    etl/           shared ingest machinery (raw stores, blob CAS,
                   render cursors) — the download side, and the one
                   place a downloader's dependencies stop.
    etl/render/    `datalib_etl_render`: the render store, the
                   unified-index load, and `RenderCtx`. Everything in
                   the tree that knows `datalib_schema` sits here or
                   above; see "Download and render are separate crates".
    etl/providers/ <p>/ (download) + <p>_render/ (render) per provider,
                   plus a <p>_config/ crate for the config schema.
                   Three providers scan local trees and share
                   etl/src/fswalk.rs (blake3 + Unison's rescan cursor):
                   fsindex (path-keyed, no render), pdf and media (both
                   content-keyed; media has no render side either), so
                   fsindex, media and lightroom have no <p>_render.
    table/         `datalib_table`: the `BulkUpsertable` row-write
                   contract, alone, with `sqlx` as its only dependency.
    migrate_config/ `datalib-migrate-config`: rewrites a `config.toml`
                   from a shape nothing writes any more into the one the
                   wizard writes. One rewrite at a time (today: ungrouped
                   steps → `[[groups]]`). The runner still *loads* the
                   old shape, with a warning naming this tool; the editor
                   cannot change it. Nothing pre-TOML is convertible any
                   more.
    runtime/       the data-root layout, the bundled-Node/npx resolver,
                   and the qmd version pin + spawn helper. Has NO
                   dependencies, deliberately: `qmd_indexer_bin` is a
                   bazel `tools=` input to the fixture's ~90s embedding
                   action, so whatever it links is the set of crates
                   whose next edit re-runs that embed on CI. `core` and
                   `unified_index` re-export from here, so the old
                   `datalib_core::layout::…` paths still resolve.
    core/          the feedback + job stores, plus re-exports of
                   `runtime`'s layout and host-runtime helpers. Knows
                   nothing about the index.
    unified_index/ the grid index, the qmd index, the query language
                   over them, and the repo that reads them. Linked by
                   datalib-step (writes it) and datalib-applet (serves
                   it) — never by datalib-http or datalib-dag.
    applets/       `datalib-applet`: the applet host, one subcommand
                   per applet (slack, unified_index). An applet
                   contributes card components and/or the endpoints
                   behind them.
    http/          `datalib-http`: API server + sync worker + UI host +
                   the applet gateway (src/applets.rs). Every route is
                   behind a per-process API token (src/auth.rs) — read
                   it from <root>/system/api-token and send
                   `Authorization: Bearer <token>`.
    schema/        hand-written row structs (grid_rows/edges/markdowns)
    app_schema/    (feedback/sync_jobs), each deriving CREATE TABLE DDL
                   via #[derive(PortableTable)].
  ui/          Vue + AG Grid frontend.
tests/         goldens under tests/__snapshots__/ (Bazel-driven).
tests/fixtures/  TNG-themed source JSON + cached `ingested/` artifact.
docs/          dev/ architecture notes; user/ guides + config_examples/;
               dev/plans/ intended work, dev/plans/completed/ landed.
third-party/   vendored upstream code (see below).
```

### Why each provider has a `<p>_config` crate

A provider's config schema lives in its own crate, holding the serde
structs and nothing else — no download code, no render code. That lets
anything needing to *understand* a config link the schema without
linking the machinery that acts on it, and it keeps the dependency rule
structural rather than merely intended.

They are **Bazel-only by design — no `Cargo.toml`**. A first-party
crate that uses only third-party dependencies the workspace already has
needs just a `BUILD.bazel` under `rules_rust`, so the crate
proliferation is close to free.

Three of them (`chatgpt_config`, `perseus_config`, `slack_config`) are
missing the comment their siblings carry; the convention applies to
them just the same.

The `<p>_render` split below is the same move for the same reason —
see §"Download and render are separate crates". Those crates *do*
carry a `Cargo.toml`, because unlike the config crates they depend on
first-party crates, so they are the ordinary case rather than the
free one.

## The sync pipeline in one paragraph

`datalib-dag <config.toml>` runs a DAG of subprocess steps. The config
has three kinds of entry: a `[[groups]]` entry is one thing on the
Manage screen (a source is a group with a `type`; the unified index is
a group without one); a `[[steps]]` entry is `group` + `function`, with
its id composed as `<group>/<function>` — the tree it writes — and
never written; an `[[applets]]` entry is a server the gateway spawns.
Edges are the declared `inputs`, which name steps by that composed id.
A built-in step writes no `command`: it runs `datalib-step`, which
reads its function and its group's `type` from the environment and
writes the tree its id names. Each source is a group with an `ingest`
step (bring the data in, from an origin or from files on disk) and a
`render_markdown` step, and two shared fan-in steps under the
`unified_index` group index every source's `render_markdown` tree:
`grid_index` (the SQL index at `unified_index/grid_index/db.doltlite_db`)
and `qmd_index` (semantic search at `unified_index/qmd_index/`). Both
are read by
the `unified_index` applet, which serves the grid — `datalib-http` does
not open them. Scheduler state lives at `system/dag_state.json`. A config entry the
loader cannot use costs that entry and nothing else — it is dropped,
the rest of the pipeline runs, and `datalib-dag --check <config>` (or
`diagnostics` on `GET /api/config`) says what went and why. A config
the app cannot serve anything from — not TOML at all, or carrying no
`unified_index` applet — comes back as `app_ready: false` and blocks
the UI behind `ConfigErrorView`, live in both directions, so a
hand-edit that breaks or fixes the file takes effect with no reload.
The http server's sync worker shells out
to `datalib-dag`; the UI's Manage tab edits the config. A root with no
config at all is the new-user case: the desktop shell's launcher
(`datalib/tauri/launcher-dist/`) offers recent roots, a folder picker,
and "create an empty one", and the app's own first-run screen
(`ui/src/views/FirstRunView.vue`) explains what `POST /api/config/init`
will write before writing it. Without that config there is no
`unified_index` applet, so the grid answers `no applet "unified_index"`
— which is what the two screens exist to prevent.
A `config.toml` written before `[[groups]]` existed is rewritten out of
band by `datalib-migrate-config`, the only place that shape is still
understood; a pre-TOML `config.yaml` root is set up again from the
app. Any executable
speaking the step protocol can be a step — see
`docs/dev/step_protocol.md`. The same config file also holds
`[[applets]]`: servers the http gateway spawns on demand to serve the
app's own components and endpoints, which the scheduler never sees
(`docs/dev/applets.md`).

## Vendored upstream: `third-party/qmd`

`third-party/qmd/` is a checked-in snapshot of
[`github.com/tobi/qmd`](https://github.com/tobi/qmd), pinned to **v2.5.3**
(see `third-party/qmd/package.json` for the authoritative version).
It exists as a **reference for the qmd format** — we don't build or ship
from it; treat it as read-only documentation in code form. Our runtime
still consumes `@tobilu/qmd` via the registry pin (`DEFAULT_QMD_VERSION`
in `datalib/backend/runtime/src/qmd.rs`): the Tauri app
bundles a pinned Node runtime plus `latchkey`/`qmd` package trees.
All three come out of Bazel — `//datalib/tauri:bundled_node`,
`//third-party/qmd/runtime:qmd_tree` and
`//third-party/latchkey/runtime:latchkey_tree` — so what the signed
app ships is what those lockfiles name; `datalib/tauri/stage-runtime.sh`
only copies them into place, and `datalib_core::node_runtime` resolves
them at run time. Every other environment — and
the app, when a pinned version isn't staged — falls back to
`npx -y @tobilu/qmd@<version>`.

### Why we don't run from the vendored tree

It looks tempting to point the indexer at `third-party/qmd/bin/qmd` for
hermeticity, but the win is smaller than it looks and was deliberately
deferred:

- The vendored tree is source-only. Running it requires `pnpm install`
  (or `bun install`) **and** `pnpm run build` to produce `dist/`. The
  install step compiles native deps (`better-sqlite3`, `node-llama-cpp`,
  `sqlite-vec`, several `tree-sitter-*`) — that's the real network and
  build cost, not the qmd fetch itself.
- We'd still need node ≥22 and a working C toolchain on the host, so
  it's not actually hermetic in the Bazel sense — just "npx-free".
- `npx`'s cache already makes repeat invocations cheap.

If we want better isolation later, the more likely direction is to
**re-implement the bits of qmd we actually use** (indexing + retrieval
against our markdown tree) in Rust inside `datalib/backend/`, using
this vendored tree purely as the format/behavior reference. That keeps
runtime deps inside the Cargo workspace and avoids growing a node
toolchain footprint.

Pulled in via `git subtree add --squash`, so the upstream tree is one
squashed commit + a merge commit in our history (no full upstream log).
To bump the pin:

```sh
git subtree pull --prefix=third-party/qmd \
  https://github.com/tobi/qmd.git <new-tag> --squash
```

Do **not** edit files under `third-party/qmd/` — they will be overwritten
on the next pull. If you need local patches, layer them outside the
subtree and document why.

## Download and render are separate crates

The step is called `ingest` and the crates below are still called
`download`; the crate, module and `DOWNLOAD.md` rename to "ingest" is
its own mechanical PR (slice 5 of
[`docs/dev/plans/groups_and_functions.md`](docs/dev/plans/groups_and_functions.md))
and has not happened.

A provider is three crates: `datalib_etl_<p>_config` holds the config
schema (§"Why each provider has a `<p>_config` crate"),
`datalib_etl_<p>` fetches, and `datalib_etl_<p>_render` turns what was
fetched into markdown and `grid_rows`. The framework splits the same
way — `datalib_etl` below, `datalib_etl_render` above it.

**The render schema stops at that line.** `datalib_schema` — `GridRow`,
`edges`, `markdowns` — is reachable from the render crates and from
nothing on the download side. That is what the split is for: moving a
`grid_rows` column used to rebuild and re-run every downloader in the
tree, including `chatgpt_live`, `claude_reset_and_redownload` and every
other test that cannot be affected by it.

The direction is enforced by Rust itself: crate dependencies are
acyclic, so a download crate *cannot* depend on its render crate even
by accident. Nothing else is needed to keep it that way, and no bazel
visibility rule is doing this job.

Two rules follow:

- **Anything a downloader needs must live on the download side.** The
  uuid recipes are the usual case: they are minted during download and
  read again during render, so they belong in `download/schema_raw.rs`
  and the render crate names them through the download crate. Before
  the split, beeper's downloader reached three of them through a
  re-export in `render/mod.rs` — which read as a render dependency and
  would now not compile.
- **A source that renders nothing has no `_render` crate at all.**
  fsindex, media and lightroom are download-only, and `download_only!`
  in `datalib_step/src/dispatch.rs` says so once rather than three
  providers each carrying a `plan_render` stub — and, with it, a
  dependency on a framework they have no use for.

The measurement that motivated the split is the one that checks it:

```sh
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

79 as of the split (105 before it). If that number climbs, something
took a dependency it should not have; the arithmetic is in
[`docs/dev/plans/completed/provider_crate_split.md`](docs/dev/plans/completed/provider_crate_split.md).

## The grid_rows union table

The Vue grid is backed by a single denormalized table, `grid_rows`,
populated by the `grid_index` step, which stacks every source's render
store (`<name>/render_markdown/indexed_markdown.doltlite_db`) into it —
asking each store `dolt_diff` since the commit the index last consumed,
so a steady-state run reads nothing. The Rust backend
(`datalib/backend/core/src/db.rs`) issues *one* SELECT against
`grid_rows` to render the grid — no per-provider branches in the query
path. The schema (column names, types, per-provider mappings) is the
hand-written `GridRow` struct in
`datalib/backend/schema/src/grid_rows.rs`; `#[derive(PortableTable)]`
produces the `CREATE TABLE` DDL from it. See `docs/dev/grid_rows.md` for
the full architecture.

When you add or change a `grid_rows` column:

1. Add the field to the `GridRow` struct in
   `datalib/backend/schema/src/grid_rows.rs` with a `#[col(sql = "…")]`
   portable type (keep the per-provider mapping in the field's doc
   comment). Index-time-derived columns use `#[derived(…)]`.
2. Update each provider's `render/grid_rows.rs` to populate the new
   column from that provider's parsed data.
3. Update the row mapper in
   `datalib/backend/unified_index/src/dolt_repo.rs` — both
   `SEARCH_ROW_COLUMNS` and `search_row_from` — plus `SearchRow` in
   `unified_index/src/search.rs` if the column reaches the API.
4. If it should be a grid column, add it to `default_columns()` in
   `datalib/backend/applets/src/unified_index/mod.rs` (which is the
   applet's wire contract, and has a test counting it) and to the
   `SearchRow` type in `datalib/ui/src/api.ts`.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

## QMDs are write-only

The render step emits QMD markdown files for human/Quarto consumption.
The backend serves those files **verbatim** (frontmatter stripped) at
`/applet/unified_index/chat/{uuid}` — it never parses them back. Structured fields
(name, account, project, channel, created_at, source_label) come from
`grid_rows` in Dolt. Per-section anchors used by the UI
(scroll-to-message, highlight, per-section feedback, copy-id) come from
`<div id="m-{uuid}" data-section-uuid="{uuid}" class="msg
msg--{provider}">` wrappers the renderer emits in the body. The UI walks
`id^="m-"` **and** `data-section-uuid` together
(`ui/src/feedback/context.ts::messageAncestor`) — those two are the
load-bearing attributes. `data-msg-index` is vestigial on the consumer
side: `DocCard.ce.vue` passes a hardcoded `0` where the feedback schema
still requires an index. Signal's renderer is the only one that still
emits the attribute. A new renderer needs the id + `data-section-uuid`
pair and nothing else. If you find yourself writing a QMD parser in the
backend, stop — add the field to `grid_rows` instead.

## Feedback persistence (doltlite)

`datalib-http` opens `<data_root>/system/feedback.doltlite_db` via
`sqlx::sqlite::SqlitePool` and wraps it — together with the jobs and
usage stores, one file each — in `AppStore`
(`datalib/backend/core/src/app_store.rs`), the implementation of the
`AppRepo` trait in `repo.rs`. The same pool serves reads and writes.

Every UUID-bearing UI surface has a "Feedback…" path. Right-click on
the grid emits `grid_cell` / `grid_row`; the search input emits
`filter_chip`; column headers emit `column_header`; the preview pane
cascades selection (`preview_selection`) → message (`preview_message`)
→ whole-thread (`page_header`); the page-header
`FeedbackButton` is `page_header`. The producer-side types and DOM
breadcrumb walker live in `datalib/ui/src/feedback/context.ts`;
the backend-side row + discriminated payload schema is the hand-written
`FeedbackRow` (+ `FeedbackContext` variants) in
`datalib/backend/app_schema/src/feedback.rs`.

Each `POST /api/feedback` inserts a row **and** runs
`SELECT dolt_commit('-Am', 'feedback: <uuid>')` on the same pooled
connection, so the commit covers exactly the row just written.

What makes that true is the **file**, not the connection. Doltlite's
working set is per-file and shared across processes, so `-Am` commits
whatever else is dirty in the same file — while `feedback` lived in the
index database that the `grid_index` step also writes, a submission
during a sync had its row swept into the step's commit and its own
commit then failed `nothing to commit`. `system/feedback.doltlite_db`
has one writer, which is what the exactness rests on. The
same-connection discipline only keeps the INSERT and the commit on one
HEAD; it isolates nothing by itself.

Bazel stamps the binary with the git hash via
`tools/workspace_status.sh` (referenced from `.bazelrc`); cargo builds
get the same value from `datalib/backend/core/build.rs`. Read-back of
feedback rows is out of scope — query the store directly with the CLI
below.

## Inspecting doltlite stores

**Stock `sqlite3` cannot open these files.** doltlite's on-disk format
is not sqlite-file-compatible; a `.doltlite_db` is a prolly-tree store
that only a doltlite-linked binary can read. Reaching for the system
`sqlite3` and concluding the database is corrupt is a well-worn dead
end.

That is about the *file*, not about getting the data out — one pipe
turns any store into a plain SQLite database that every SQLite tool
reads, and a user has the shell to do it with, because
`datalib-doltlite` ships in the release tarball:

```sh
datalib-doltlite -readonly <store>.doltlite_db .dump | sqlite3 out.sqlite
```

Say that alongside the warning whenever you write the warning down.
Stating the limit without the escape hatch is what made a reviewer
call the format a re-siloing of the data; the full recipe, and what
the snapshot does and doesn't carry, is in
[`docs/dev/doltlite.md`](docs/dev/doltlite.md).

For work inside a checkout, use the Bazel-built shell, which links the
same amalgamation the Rust binaries do:

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite

$dl path/to/db.doltlite_db ".tables"
$dl path/to/db.doltlite_db ".schema grid_rows"
$dl path/to/db.doltlite_db "SELECT provider, COUNT(*) FROM grid_rows GROUP BY provider;"
$dl path/to/db.doltlite_db "SELECT COUNT(*) FROM dolt_log;"   # commit history
```

It is a sqlite3-shell drop-in, so dot-commands, `-json`, `-csv` and an
interactive REPL all work, plus the dolt SQL surface
(`dolt_commit`, `dolt_log`, `dolt_diff`, …).

Where the stores live under a data root:

```
<data_root>/<group>/ingest/entities.doltlite_db  per-source entities + sync bookkeeping
<data_root>/<group>/ingest/blobs.doltlite_db     content-addressed blobs
<data_root>/<group>/render_markdown/…            the rendered tree + its render store
<data_root>/unified_index/grid_index/db.doltlite_db   grid_rows / markdowns / edges
<data_root>/system/feedback.doltlite_db         filed feedback
<data_root>/system/jobs.doltlite_db             the sync job queue
<data_root>/system/usage.doltlite_db            bytes-on-disk over time
```

One writer per file, and it is load-bearing: doltlite's working set is
per *file* and shared across processes, so two writers on one file
commit each other's in-flight rows. The `grid_index` step owns the
index; `datalib-http` owns feedback, jobs and usage; the applet only
reads.

`usage.doltlite_db` is the one store nothing ever commits. It is a
timeseries — `datalib-http` walks the root every five seconds *while a
run holds it* and appends a row per tree whose size moved — so the rows
*are* the history, and a `dolt_commit` per sample would flood
`dolt_log` with nothing the table doesn't already say. The gate matters
when you read it: between runs nothing writes the root, so the series
deliberately has no samples there, and a change made from outside
datalib carries the instant it was next *measured* rather than the
instant it happened. It has its own file for exactly
the reason the others do: a `-Am` commit from the job store would
otherwise sweep whatever samples happened to be dirty into it. Reading
it is `SELECT path, measured_at, bytes FROM disk_usage`; note it is
compacted (no repeated value, nothing closer than five seconds), so
carry the last value forward rather than assuming a fixed interval.

Two other copies of the same shell exist. `datalib-doltlite` is the one
a released install has (it is in `:dist`), and there is a host
`/usr/local/bin/doltlite` on some machines. When you are working in a
checkout, prefer the Bazel target over both: it is version-locked to
`MODULE.bazel`'s pin, so it can't silently disagree with what the tree
you are editing writes.

**From a test**, take it as a `data` dep and pass `$(rootpath ...)`
rather than shelling out to a host binary — that keeps the test
hermetic. `//tests/fixtures:ingested_tng_test` is the worked example:
it opens the stores the pipeline just wrote and asserts row counts and
per-provider coverage. Prefer that over grepping tracing events out of
stderr; a log line tells you what the code *said*, the store tells you
what it *did*.

## One open per doltlite file, and close it before the next

Every pool against a `.doltlite_db` is `max_connections(1)`, and doltlite's
working set lives in the **file** rather than in the connection. A second
pool is therefore not a second view of the store; it is a second handle on
one shared uncommitted tree, and a `dolt_commit('-Am')` through either one
sweeps up whatever the other has in flight.

**The open itself does not wait.** Measured on macOS with doltlite 0.50.3
by `//datalib/backend/etl:doltlite_two_process_test`: a second read-write
pool on an already-open file opens in ~2ms and both pools then commit, and
a read-only pool alongside a live writer — opened in either order — costs
each other nothing. What overlap costs you is the shared working set above,
plus contention while two pools are actually mid-write. So a hang here is
not the open blocking; it is two writers on one tree.

**Two writers mid-write is the second face, and it errors rather than
waits.** Those measurements commit through each pool in turn. Commit
through both *at once* and one of them fails outright with `commit
conflict: another connection committed to this branch` — naming a commit
that need not have happened; read it as "someone else has this store open
right now". The asymmetry is in the source: ordinary DML retries under a
busy handler (`btreeBeginTrans` loops on `prollyInvokeBusyHandler`) and
rides the overlap out, while `dolt_commit` takes the store's lock once
(`csFileLockNB`, via `RefreshAndConfirmHead`) and gives up if a peer holds
it. `doltlite_raw::open` commits three times on the way in, so an
overlapping *open* fails inside `open` itself, reported as `commit schema
after DDL`. `two_live_pools_on_one_store_break_each_others_commits` in
`doltlite_raw.rs` pins this half.

Three rules follow, and none is optional:

- **Open the store once per pass.** If a stage needs to load rows, run a
  `dolt_diff` scan and probe for missing ids, all three go on the one
  pool. Reaching for a fresh `open_reader` per question reads as harmless
  and is not.
- **`close().await` before the next open**, on the error path too.
  Dropping the handle only *schedules* the disconnect, so a `?` between
  two opens leaves them overlapping.
- **A download takes the store as an input.** Every provider's
  `FetchOptions` carries `pub db: RawDb`; `fetch` never opens one, and
  whoever opened it closes it. `lint_repo.py`'s check 6 enforces this.
  See `datalib/backend/etl/README.md` for what the `Option<RawDb>` this
  replaced actually cost.

Render additionally reads through `open_reader`, never `open`: the write
path rescue-commits, reconciles the schema and commits with `-Am`, which
is three writes to a store the render step does not own. `lint_repo.py`'s
check 5 enforces that half; nothing enforces the first two rules above.

**Expect this to pass locally and fail on CI.** Whether overlapping pools
actually collide depends on timing and on the filesystem's locking, so a
mac laptop and a Linux CI container disagree readily. A render path that
opened three pools per pass ran in 10s here and hit the 300s timeout on
`//tests/fixtures:ingested_tng_test` there (#311). A download that opened
its own pool and never closed it produced intermittent `commit conflict`
failures across the doltlite-heavy targets (#327). The measurements above
are macOS only, and that is exactly the platform this warning says not to
trust: if a doltlite-touching change is green locally and red or slow in
CI, count the opens first.

## Git: prefer merges over rebases

When integrating remote changes into a local branch (e.g. `git pull` after
a rejected push), **prefer a merge commit over a rebase**. Rebasing
rewrites local commit hashes, which loses the "what actually happened"
history and can surprise other clones. A merge commit keeps both sides of
the history intact and is cheap to read with `git log --first-parent`.

In practice: `git pull` (default merge), not `git pull --rebase`. Force-
push is off the table on shared branches.

## Python deps: pyproject.toml → requirements.txt → Bazel

`uv` and Bazel read **different** files for Python deps:

- `uv run …` reads `pyproject.toml` + `uv.lock`.
- Bazel's `pip.parse` in `MODULE.bazel` reads `requirements.txt` (the
  hub is `@py_pip`, consumed via `requirement("…")` in BUILD files).

`requirements.txt` is a generated artifact — it must be regenerated
after any `pyproject.toml` dep change, or Bazel targets that try to
`requirement("newpkg")` will fail with
`no such package '@@…py_pip//newpkg': BUILD file not found`:

```sh
uv export --no-emit-project --no-emit-workspace --format requirements-txt -o requirements.txt
```

Then add `requirement("newpkg")` to the relevant `BUILD.bazel` `deps`.
A `uv run` smoke test won't catch a missing Bazel dep — the venv has it.
Run `bazelisk build //…` to verify. Python is only used for fixture /
test-pipeline tooling (`tests/fixtures/`) and scripts; everything in the
shipping path is Rust.

## Running tests

**"Build green" means `bazelisk test //...` passes — nothing less.** A
narrower *bazel* invocation (`bazelisk test //some/subtree/...`, a single
target's tests) is fine for inner-loop iteration, but don't call the tree
green based on one of those. If you report "build green" without having run
`bazelisk test //...`, say what you actually ran instead.

**But `bazelisk test //...` is not the whole CI gate.** The `bazel test`
job runs a **repo hygiene lint step first** and skips the tests entirely
if it fails — so a tree can be green by the paragraph above and still get
a red cross, with the test results never printed. It cannot be a Bazel
*test*: `scripts/lint_repo.py` has to enumerate every tracked file via
`git ls-files`, which is exactly what a sandbox exists to prevent. Its two checks are that every `no-sandbox` tag is
allowlisted, and that every first-party `*.py` sits under a Python lint
root so ruff and pyright actually see it.

So the complete local gate is the hygiene lint **and** the test suite:

```bash
bazelisk run //:lint_repo && bazelisk test //...
```

`bazelisk run //:precommit` runs the same lint plus clippy, and is the
friendlier wrapper if you want everything. Both go through
[`//:lint_repo`](BUILD.bazel), a `py_binary` — deliberately, so the
script runs on Bazel's pinned Python rather than the host's. It needs
`tomllib` (Python ≥3.11) and macOS still ships 3.9 as `python3`, which
used to make `//:precommit` die with a bare `ModuleNotFoundError` on
every Mac.

**Bazel is the only supported build/test driver — don't shell out to
`cargo test` / `cargo build` / `pnpm test` for the inner loop.** They
bypass Bazel's action cache (so they neither use nor warm it) and its
sandboxing, and risk producing artifacts that disagree with what CI sees.
If your inner loop feels slow, narrow the bazel invocation or fix the
slow target — don't drop to cargo.

**Coverage** uses `bazelisk coverage` with a one-shot wrapper that
captures Rust-subprocess hit counts too — see
[`docs/dev/coverage.md`](/docs/dev/coverage.md). The short form:

```bash
tools/run_coverage.sh //tests/fixtures:ingested_tng_test -- \
  //datalib/backend/dag:datalib_dag_bin \
  //datalib/backend/datalib_step:datalib_step \
  //datalib/backend/signal-backup:signal_make_fixture
```

**Run the cheap tests locally; let CI run the full suite.** `bazelisk
test //...` is still the source of truth, but this repo is public, so
CI's runners are free and unmetered while your laptop's are not. **A
green CI run of `//...` satisfies that rule; a narrower local run does
not.** Measured on one warm mac:

| loop | command | cost |
|---|---|---|
| lint + typecheck | `bazelisk test //:lint` | **~3s** |
| every hermetic test | `bazelisk test //... --build_tests_only --test_tag_filters=-no-sandbox,-requires-network,-external,-manual` | **~106s** after edits to a shared crate, **~2s** when nothing moved; 133 of 146 targets |
| the package you're editing | `bazelisk test //datalib/backend/etl/...` | varies |
| the whole gate, e2e included | push, and read CI | ~3 min warm / ~20 min cold |

Reach for the middle row before pushing. It drops the 13 targets that
need a host, and `--build_tests_only` stops it building the rest of the
tree to run them. **Those tag filters belong on that line and nowhere
else** — never on the full run; the paragraph below says why.

Don't shell out to `cargo` / `pnpm` for any of these — they bypass the
cache and can disagree with CI.

The disk cache is shared by every worktree (one absolute path, see
`.bazelrc`), so size its cap against how many you keep live. Below their
sum they evict each other and every worktree switch recompiles. Check
`du -sh ~/Library/Caches/bazel-disk-cache`; sitting *at* the cap is the
symptom.

**Do not add `--test_tag_filters=-manual,-external` to the FULL run**
(the last row of the table above — the one whose green is what "build
green" means). The canonical line is the bare `bazelisk test //...`. Filtering on
`-external` silently drops `//datalib/ui:e2e_test` (Playwright), which
lets UI regressions through. (The lint/typecheck gate — `//:lint`, i.e.
ruff + pyright + vue-tsc — is fully hermetic and carries no tags, so no
filter can drop it; clippy, fmt and the unused-dependency check ride
the always-on rustfmt aspect, the always-on clippy aspect and
`per_crate_rustc_flag` respectively — see the `.bazelrc` comments.) If a test is host- or
network-dependent it's tagged `requires-network` and/or `no-sandbox`,
which Bazel respects on its own — `external` is reserved for tests
that hit third-party services you don't want CI talking to. Prefer
`bazelisk` over `bazel` so the workspace's pinned Bazel version wins.

**Beware consuming Bazel outputs from outside Bazel**: anything that
reads `bazel-bin/tests/fixtures/ingested/*` is reading a genrule output.
Tools outside Bazel don't know how to rebuild it, so if you change any
download/render/schema code and re-run outside Bazel, you'll compare
fresh results against a stale artifact and chase phantom failures. Go
through bazel (`bazelisk test //tests/fixtures:ingested_tng_test`, or
`//...`) so the fixture is rebuilt first.

There is no `//tests:test_snapshots` target — this paragraph used to
send you to one, and to a `dump.sql` that the fixture stopped producing.
`tests/` holds only `fixtures/` (checked 2026-08-20). Provider-level
insta snapshots are the golden tests that do exist; see the
`.update` targets below.

### Updating insta snapshots (`.update` targets)

`bazel test` runs each action in a sandbox, so plain
`--test_env=INSTA_UPDATE=always` lands new `*.snap` files inside the
sandbox where they can't be reviewed. The standard fix is to invoke
the update via `bazel run` against a sibling `.update` target. Every
insta-using `rust_test` in this tree has one declared via the
`insta_update` macro in `//tools:insta.bzl`:

```bash
# Hermetic snapshot tests — no host prereqs.
bazel run //datalib/backend/unified_index:fixture_db_snapshot_test.update
bazel run //datalib/backend/etl/providers/chatgpt:chatgpt_render.update
bazel run //datalib/backend/etl/providers/slack:slack_translate.update

# Live tests — need LATCHKEY_CURL on the host (same as cargo). Builds
# the shim once:
bazel build //datalib/backend/etl:latchkey_curl_impersonate
export LATCHKEY_CURL="$(pwd)/bazel-bin/datalib/backend/etl/latchkey_curl_impersonate"
bazel run //datalib/backend/etl/providers/claude:claude_live.update
```

The wrapper sets `INSTA_WORKSPACE_ROOT=$BUILD_WORKSPACE_DIRECTORY`,
which only exists under `bazel run` and resolves to the source tree
(not the sandbox), so insta writes — including brand-new `.snap`
files — land where `git status` will show them. Always review the
diff before committing.

When adding a new insta-using test, declare a sibling `.update`:

```python
load("//tools:insta.bzl", "insta_update")

rust_test(
    name = "my_render_test",
    data = [":tng_fixture"],
    env = {"MY_FIXTURE_DIR": "datalib/.../fixtures/my_api"},
    ...
)

insta_update(
    name = "my_render_test.update",
    test = ":my_render_test",
    test_args = ["--ignored"],  # only if the test is #[ignore]'d
    # `data` and `env` on rust_test DO NOT propagate through the
    # sibling sh_binary wrapper — mirror every fixture / env-var dep
    # here or `bazel run …update` will panic with "fixture not found".
    extra_data = [":tng_fixture"],
    extra_env = {"MY_FIXTURE_DIR": "datalib/.../fixtures/my_api"},
)
```

### "Why was CI slow?" — read the BuildBuddy invocation

Both `test.yml` jobs post to our BuildBuddy org at `imbue.buildbuddy.io`
(configured by `.github/actions/prepare-bazel`; the key lands in the
gitignored `.bazelrc.user`, and `--config=buildbuddy` in `.bazelrc`
turns it on). **On `main` the whole build is essentially a cache
replay** — so any run that takes noticeably longer is telling you what
it had to rebuild, and that is the question worth asking.

Every bazel invocation prints its own dashboard link. Pull it out of
the job log:

```bash
gh api "repos/imbue-ai/datalib/actions/runs/<run-id>/jobs" \
  --jq '.jobs[] | select(.name=="bazel test //...") | .id'
gh api "repos/imbue-ai/datalib/actions/jobs/<job-id>/logs" > /tmp/ci.log
grep -oE 'https://imbue\.buildbuddy\.io/invocation/[a-f0-9-]+' /tmp/ci.log | sort -u
```

The log is only served **after the job finishes** — while it is running
the API returns "still in progress", so poll
`gh api repos/imbue-ai/datalib/actions/jobs/<id> --jq .status` in the
background rather than blocking on it.

Three lines in that log answer "what work was actually done", and
comparing them against a `main` run is usually the whole diagnosis:

```bash
grep -E 'INFO: Elapsed time|processes:|Executed [0-9]+ out of' /tmp/ci.log
```

  * `N processes: A remote cache hit, B internal, C local, D
    processwrapper-sandbox` — **`D` is the real signal.** Sandboxed
    actions are the ones that actually compiled or ran; cache hits and
    `internal` are free.
  * `Executed N out of M tests` — how many tests really ran. On a warm
    `main` this is **0**.
  * `Critical Path` — the serial floor. Wall-clock can't go below it.

Worked example, the three runs compared while diagnosing #208:

| run | elapsed | critical path | sandboxed | tests executed |
|---|---|---|---|---|
| `main` @ `d01e30b8` | 175s | 46s | 0 | **0 of 114** |
| a one-provider PR | 142s | 74s | 27 | **8 of 114** |
| #208 (touched shared `datalib_etl`) | **1016s** | **310s** | **322** | **59 of 115** |

The cause is blast radius, and you can measure it before pushing —
which is the point of writing this down. `rdeps` says how much of the
tree a file's crate is upstream of:

```bash
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl:datalib_etl))'   # 80 test targets
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'  # 96 — two thirds of the suite
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl/providers/slack:datalib_etl_slack))'  # 12
bazelisk query 'kind(".*_test", rdeps(//..., //tests/fixtures:ingested_tng))'       # 3, incl. the 42s e2e suite
```

#208 added a 20-line helper to `datalib/backend/etl/src/doltlite_raw.rs`
— the crate 130 targets depend on — so ~300 actions that are normally
cache hits had to be rebuilt and 59 test targets re-run. **This is a
one-time cost, not a regression:** once the commit is on `main` the
cache is warm and later PRs drop back to ~3m. It is worth knowing about
mainly so you can (a) not panic, and (b) decide deliberately whether a
small helper really belongs in a shared crate — the `rdeps` number is
the price tag.

**Runs are bimodal, so ask which mode you are in first.** A warm run
executes 0 tests and takes ~3 min; a cold one rebuilds ~345 actions and
takes ~20, with almost nothing in between. A rising *median* therefore
usually means cold runs got more frequent, not that anything got slower.
It is **not** the e2e suite: on a 1254s cold run every executed test
together came to 200s. The rest is opt-mode Rust, and blast radius is
the only lever on it.

A run can also be slow without compiling anything — check whether the
job *started* late (`created_at` vs the job's `started_at`) before
reading any of the numbers above. That is runner queueing, and none of
this applies to it.

`--config=remote` (`.bazelrc`) sends the compiles to BuildBuddy remote
execution instead of the runner's 4 vCPUs; `test.yml` takes it via a
`remote_execution` dispatch input. It is a **trial switch, not the merge
gate** — #324 holds the measurements and the open decision.

Not exercised here, so treat as a pointer rather than a recipe:
BuildBuddy also has a REST API and a side-by-side invocation compare in
its web UI. Both need an API key (`https://imbue.buildbuddy.io/settings`),
which CI has and a local checkout does not by default.

#### Locally you are probably *not* on the remote cache

Two things hide this, so check rather than assume:

  * `.bazelrc` gives everyone a **machine-wide disk cache**
    (`build --disk_cache=~/Library/Caches/bazel-disk-cache`). It is an
    absolute path, so every checkout and every worktree shares it, and
    it makes local builds feel fast — but only for actions *you* have
    built before. Nothing CI built ever lands in it.
  * The remote cache needs `.bazelrc.user`, which is **gitignored and
    per-workspace**. `try-import %workspace%/.bazelrc.user` resolves to
    the *worktree* root, not the main checkout, so a file you created
    once in `datalib/` is invisible to every `.claude/worktrees/*`
    clone. Both facts together mean a tree can look configured and not
    be. Confirm with:

```bash
grep -c buildbuddy .bazelrc.user 2>/dev/null || echo "no .bazelrc.user in THIS workspace"
```

**Even with the key, a mac shares almost nothing with CI.** An action's
cache key covers its toolchain and target, so a darwin-arm64 rustc
action and CI's linux-x86_64 one are different actions and neither can
hit the other's entry. Locally the remote cache buys you sharing with
your *own* other worktrees and machines, plus repository fetches through
the remote downloader — not a replay of CI's work. For the same reason
`.bazelrc`'s `remote` config (BuildBuddy remote *execution*) is CI-only:
the autodetected cc toolchain is generated from the client host, so
driving Linux executors from a mac hands them a darwin toolchain.

The `processes:` line settles it either way. A run on the remote cache
names it — CI's reads `4070 remote cache hit, …`. A local run without
`.bazelrc.user` never does; it reports only local buckets, e.g.
`1 process: 63 action cache hit, 1 internal` or `… 2 disk cache hit,
26 darwin-sandbox`. **The tell is the absence of `remote cache hit`,
not the presence of any particular local bucket** — which of them
appears varies with what the run had to do.

Keep one real file outside the repo and symlink it in, so a new
worktree is one command rather than a re-paste of the key:

```bash
mkdir -p ~/.config/datalib && chmod 700 ~/.config/datalib
cat > ~/.config/datalib/bazelrc.user <<'EOF'
common --remote_header=x-buildbuddy-api-key=<your-key>
build --config=buildbuddy
EOF
chmod 600 ~/.config/datalib/bazelrc.user

# link it into the main checkout and every worktree
for d in . .claude/worktrees/*/; do
    ln -sfn ~/.config/datalib/bazelrc.user "$d/.bazelrc.user"
done
```

Don't put `build --config=buildbuddy` in `$HOME/.bazelrc`: the home rc
applies to *every* bazel workspace on the machine, and the
`buildbuddy` config is only defined in this repo's `.bazelrc`, so
unrelated projects would fail with "Config value 'buildbuddy' is not
defined in any .rc file".

### "Which tests are flaky?" — read the reruns

Hitting "re-run failed jobs" replays the same commit, so a commit that
carries both a failure and a success flaked. `scripts/flaky_tests.py`
groups GitHub Actions runs by commit, keeps the mixed ones, and reads
the failed attempt's log for bazel's `FAILED` summary, so you get target
names and a BuildBuddy link per episode rather than "CI was red":

```bash
scripts/flaky_tests.py --limit 400
```

It only sees flakes somebody actually re-ran — a red PR that got an
empty commit pushed at it instead leaves no trace — and GitHub deletes
run logs after 90 days, past which an episode still counts but its
target names are gone.

## Common commands

```bash
# Source of truth — run this before claiming tests pass
bazelisk test //...

# Narrower inner loop (faster) — still bazel, so the cache stays warm
bazelisk test //datalib/backend/...

# Rebuild the fixture ingest (dump.sql + qmd.tar)
bazelisk build //tests/fixtures:ingested_tng

# Stage every shipped binary under its public dash-separated name, then
# run a pipeline against a data root's config (no --binary-dir needed:
# datalib-dag falls back to its own directory to find datalib-step)
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-dag <data_root>/config.toml
```

## Provenance: `claude_api` vs `claude_export`

Claude data can come from the live web API (`type: claude_api`) or an
unpacked bulk export (`type: claude_export`) — two separate source
types, each its own download + render step pair, both served by one
provider crate.

**They write the same raw store.** `claude_api` walks the API and
`claude_export` reads the export's JSON off `common.input_path`, and
both land rows in the same six tables of `<name>/ingest`, so the render
step has exactly one input shape. The API downloader gets there by
normalizing every response into the bulk-export on-disk shape
(`normalize_to_export_shape` in
`datalib/backend/etl/providers/claude/src/download/normalize.rs`,
stamping `_source: { via: "claude.ai/api", org_uuid }` provenance); the
export ingest stores what the export already said, with the org columns
NULL — which is how the renderer tells the two apart and knows not to
normalize an already-normalized payload a second time.

Until #207 the export type had no download wave at all: the renderer
read the export tree in place through a second parser, and the source
had no raw store, no `sync_runs` row and no way to notice a deleted
conversation. If you find prose calling `claude_export` "render-only",
it predates that fix.

Because the two types share a store, seeding one from an export and
then keeping it fresh with the API nearly works today — and has one
destructive edge (the export ingest prunes to its own snapshot, so
re-running it over an API-extended store deletes what the API added).
Read DOWNLOAD.md's "Bootstrapping from an export" section before trying
it. See `datalib/backend/etl/providers/claude/DOWNLOAD.md`.

### "Claude", not "Anthropic"

**Claude is the product; that is the name we use.** The provider crate is
`datalib_etl_claude` under `providers/claude/`, the source types are
`claude_api` / `claude_export`, the `grid_rows.provider` tag is
`claude`, the tables are `claude_attachments`, processor ids are
`claude/<name>/…`, and tracing events are `claude_*`.

The rule that settled it is the sibling comparison, not a headcount:
**every source type in this tree is named for the product a person
recognizes, never for the vendor** — `chatgpt_api`, not `openai_api`;
`lightroom`, not `adobe_catalog`. The provider directory was the single
exception until #269, where `anthropic` sat next to `chatgpt` and named
the company instead of the thing.

Write **Anthropic** only where you mean the company or something it
issues, because there the distinction is real and load-bearing:

- "Anthropic issues stable UUIDs for every entity" — a fact about
  upstream, and the reason `Scope::ProviderGlobal` is safe here.
- "the owning Anthropic organization" — `org_uuid` is an org *in
  Anthropic's account system*, not in ours.
- "if Anthropic ever ships attachment bytes inside the export".

The one deliberate survivor of the rename is the `anthropic` **search
keyword** in `ui/src/config/catalog.ts`: someone who thinks of the
company should still find the source in the picker.

## Unordered collections: give a bag an order before storing it

**A JSON array is not necessarily a list.** When an API returns a *set*
— capabilities, permissions, tags, member ids, labels — the array order
is whatever the server happened to emit, and nothing promises it is
stable between fetches. Sort it before it goes into a content payload.

This is an architectural rule, not tidiness: the whole pipeline's
incrementality rests on an unchanged record serializing identically to
itself. The argument is in
[`data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md#efficiently-incremental).

Left unsorted, a re-fetch of an unchanged object serializes differently
from itself, and everything downstream believes it changed:
`dolt_diff_<table>` reports a modification, the entity re-renders, and
the manual-e2e golden's `--reset-and-redownload` stability check fails
on content that never moved. Found this way on 2026-08-31 — claude.ai
returns a project's eight `permissions` strings in a different order on
different fetches (`canonicalize_project_payload` in the Claude
downloader now sorts them).

**Sort; don't declare it volatile.** The two look interchangeable and
are not. `*_VOLATILE_PATHS` says *"this field's value carries no
information"* and drops it from the content payload — right for a
per-fetch `updated` stamp. For a bag the contents are content — losing
a permission is a real change you want to see — and it is only the
order that means nothing. Sorting keeps the signal and removes the
noise; declaring it volatile throws the signal away too.

Applies to nested arrays as well, and the sort has to be total: sort by
the rendered string rather than by `as_str()`, so a mixed-type array
gets an order instead of a panic.

## Name a closed set of strings

**If a string can only be one of a handful of values, it should be an
enum.** A bare `&str` or `String` in that position gives you nothing: no
list of what the values are, no place to say what one *means*, no
compile error when a `match` misses one, and "find references" returns
every unrelated use of the same word.

Rust has no `StrEnum`, so use [`strum`](https://docs.rs/strum) — it is
already a workspace dependency:

```rust
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(EnumString, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunState {
    /// Invoked, and the scheduler is waiting on it.
    Running,
    /// In the runnable subgraph, but up to date. Checked, and current.
    SkippedUpToDate,
    …
}

impl RunState {
    pub fn as_str(self) -> &'static str { self.into() }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> { s.parse().ok() }
}
```

`VariantArray` gives `RunState::VARIANTS`, so nothing has to re-list the
values — that list *is* the enum, and it cannot silently miss one. The
two thin `as_str` / `parse` wrappers are the house idiom; keep them so
call sites read as `RunState::Failed.as_str()` rather than a bare
`.into()`.

Two rules for the boundary:

- **`parse` returns `Option`, never a guess.** A store written by a
  newer build, or a third-party step, can name a value this binary does
  not have. The caller decides what that means — see `TaskState::for_run_state`,
  which maps an unknown status to `Failed` *deliberately*, with a
  sentence saying why.
- **Add a test that strum and serde agree** when a type derives both.
  They are independent derives producing independent strings, so the
  agreement is a real check, not a tautology. One `#[test]` over
  `VARIANTS` covers it.

Where the value is already stored as a `VARCHAR` or a JSON string, leave
the storage type alone and route every read and write through the enum.
In SQL that means **binding** the value, not interpolating it — the
statement stays a `&'static str` and needs no `AssertSqlSafe`:

```rust
sqlx::query("UPDATE sync_jobs SET state = ? WHERE id = ? AND state = ?")
    .bind(JobState::Canceled.as_str())
    .bind(job_id)
    .bind(JobState::Pending.as_str())
```

### When a string really is a string

Don't reach for an enum when the set is not closed and not ours:

- **Values that come from upstream.** Notion block types, MIME types,
  Matrix event types. Match on them at the boundary and convert to
  something of ours; the arms are a parser, not a vocabulary.
- **Free-form display text.** `grid_rows.kind` is a per-provider label
  (`"Slack Message"`, `"Notion Page"`, `format!("Chapter ({id})")`).
  Deliberately open.
- **JSON keys and SQL identifiers.** `"uuid"`, `"created_at"`. A name,
  not a value.

### Where the vocabularies are

One enum per vocabulary, living with whoever mints it:

| vocabulary | type | home |
|---|---|---|
| what a step is doing in a run | `RunState` | `dag/src/run_state.rs` |
| why a step failed | `FailureKind` | `dag/src/step.rs` |
| what the progress bus itself names | `LiveState` | `progress/src/lib.rs` |
| a sync job's lifecycle | `JobState`, `JobKind` | `app_schema/src/sync_jobs.rs` |
| a task board row | `TaskState` | `http/src/worker.rs` |
| a browser-login attempt | `ConnectState` | `http/src/connect.rs` |
| the `grid_rows.provider` tag | `Provider` | `schema/src/providers.rs` |
| what render could not do | `Outcome`, `Reason`, `ScopeKind`, `Stage` | `schema/src/render_problems.rs` |
| a config's `[[steps]]` source type | `SourceType` | `datalib_step/src/source_type.rs` |

The TypeScript side mirrors these as string-literal unions in
`datalib/ui/src/api.ts` (`DagRunState`, `SyncTaskState`, `SyncJobState`,
`ConnectState`). They are hand-kept in step with the Rust — there is no
generator — so change both halves together.

## A `deps` entry you don't use is a build error

Every `deps` / `proc_macro_deps` entry under `datalib/` must actually be
used by the crate that names it. rustc is handed the exact `--extern`
set by bazel and knows which ones it resolved a path through, so this
needs no separate tool — one `.bazelrc` line turns it on, and the
comment there explains why it must be `per_crate_rustc_flag` rather
than `extra_rustc_flag` (the global form also lands on third-party
crates, whose dep lists we cannot fix).

Two things to know when it fires:

- **A dep used only under `#[cfg(test)]` is reported unused on the
  library**, because the library build never compiles that code. Move
  it from the `rust_library`'s `deps` to the `rust_test` that names the
  library with `crate = `. That is where it was really needed, so the
  graph gets more accurate rather than merely shorter.
- **A dep that is genuinely needed but never named** — a linker
  artifact, say — is kept with `use <crate> as _;` in the crate root,
  which is what rustc's own help text suggests. Nothing here needs that
  today.

## Fallbacks: prefer failing loudly to succeeding quietly

**Avoid fallbacks.** The dangerous ones *succeed*: a correct answer
reached the slow or lossy way raises no error, so an assumption that
expired weeks ago hides behind a vague "feels slow". If you add one
anyway, log when it fires. Worked example: #225 — the DAG runner spent
40s hashing 3.4GB to version a step it had already skipped, on every
run, for two weeks, before anyone noticed.

## Dynamic SQL needs `AssertSqlSafe` and a reason

sqlx 0.9 only accepts `&'static str` as a query string. Anything built
at runtime — a `?,?,?` run sized from a chunk, a `{table}` interpolated
as an identifier — has to be wrapped in `sqlx::AssertSqlSafe(...)`,
which is an assertion *you* are making, not a check sqlx performs.

Wrap it with a comment saying why it is safe, the way the existing
sites do. Two patterns cover almost everything here: placeholders built
from a count with every value bound, and table/column names that are
`&'static str` at every callsite. If yours is neither — you are
interpolating something that came from upstream data — quote it
(`lightroom`'s `plan::quote_ident`) or bind it instead.

## Timestamp convention

Every timestamp stored anywhere in this project — Dolt columns, JSON cache
files, QMD frontmatter — is an **ISO-8601 string that preserves the
timezone offset present in the source**.

- If the upstream API gave us `2026-05-04T03:42:05-07:00`, we store
  `2026-05-04T03:42:05-07:00` verbatim. Don't normalize to UTC — the local
  offset itself carries information (it's how the timestamp would have
  rendered to the human who saw it), and once dropped we can't get it back.
- If the upstream gave us `...Z`, leave it as `Z` — that's still a valid
  offset.
- If the upstream gave us a unix-epoch number (no source offset), render
  it as UTC with an explicit `+00:00` suffix, e.g. `2026-05-04T10:42:05.123456+00:00`.
  Use `datetime.fromtimestamp(t, tz=timezone.utc).isoformat()` —
  *not* `.strftime("...Z")`.
- For our own "now" timestamps (`first_seen_at`, `last_seen_at`,
  ingest-started markers, `_fetched_at`): use **local** time with explicit
  offset, `datetime.now().astimezone().isoformat()`. The local offset is
  itself information — it tells future-you what wall-clock time the ingest
  happened in the zone where it actually ran. Don't normalize to UTC.
  Steps should prefer the run-pinned `DATALIB_DAG_NOW` over sampling
  their own clock, so one run's outputs agree.

If you find yourself writing `strftime("%Y-%m-%dT%H:%M:%SZ")`, stop and
use `isoformat()` instead. The columns are `VARCHAR(40)`, wide enough for
the longest offset-suffixed form including microseconds.

## Auth (web API)

The Rust downloaders under `datalib/backend/etl/providers/*/src/download/`
read the `sessionKey` cookie out of `latchkey curl -v` stderr and then
issue the actual requests via the `latchkey-curl-impersonate` so Cloudflare's
JA3 wall passes. If the cookie is missing or expired,
`latchkey auth set claude-ai` fixes it; if Cloudflare still 403s, the
IP/UA may be flagged — wait it out or swap networks.
