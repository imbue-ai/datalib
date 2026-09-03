# datalib — agent runbook

Quick references for AI/human contributors working **on the datalib
codebase**: where the docs are, how the repo is laid out, and the
conventions that aren't obvious from the code. If you are an agent
*using* datalib (running syncs, querying a user's mirror, writing a
custom step), start with [`agent_user.md`](docs/agent_user.md) instead.

## Doc map

Start here when a task touches an area you don't already know. All paths
are relative to the repo root.

**Pipeline / sync engine**

- [`docs/dev/pipeline_dag_architecture.md`](docs/dev/pipeline_dag_architecture.md)
  — how the sync pipeline works: the `datalib-dag` runner, step contract,
  scheduler (edge derivation, skipping, retry, subtree poisoning), and
  the implementation decisions.
- [`docs/dev/step_identity.md`](docs/dev/step_identity.md) — *proposal*:
  making a step's `id` the path it writes, so `inputs` name step ids and
  the six places that recover an identity by splitting a string go away.
  Nothing in it is built; the `name` / `id` split that did ship is in
  `source_wizard.md`.
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
- [`configs/dag_example.toml`](configs/dag_example.toml) — a complete,
  commented steps-format config, including the recipe for running
  `datalib-dag` from a bazel build.

**Data architecture**

- [`docs/dev/data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md)
  — the download (ingestion) architecture: raw stores, incrementality,
  resumability, wire tape. Companion:
  [`data_architecture_ingestion_practices.md`](docs/dev/data_architecture_ingestion_practices.md)
  (how to build a new provider).
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
  union table behind the grid UI.
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
- [`docs/dev/multimodal_retrieval.md`](docs/dev/multimodal_retrieval.md)
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
- [`docs/dev/wizard_file_pickers.md`](docs/dev/wizard_file_pickers.md)
  — **read before adding a source to the Add/Edit wizard**: a field
  that asks for a file or folder must offer a native OS picker, not a
  text box. How the three layers fit (Tauri capability → `pickPath` →
  the button), the checklist for a new path field, and why the
  browser-served case can't have one. The wizard's own design is
  [`docs/dev/source_wizard.md`](docs/dev/source_wizard.md) (a
  proposal, only partly built — read its banner); the descriptors you
  actually edit are `datalib/ui/src/config/catalog.ts`.
- [`docs/dev/qmd_index_ui.md`](docs/dev/qmd_index_ui.md) — the grid's
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

**User-facing**

- [`docs/user/first_time_user.md`](docs/user/first_time_user.md),
  [`docs/user/getting_your_data.md`](docs/user/getting_your_data.md),
  and [`docs/user/config_examples/`](docs/user/config_examples/) (one
  commented `<name>.download` + `<name>.render` step pair per source,
  in the steps format).

**Historical** — [`docs/dev/archived/`](docs/dev/archived/) holds
point-in-time plans and audits (each with an "Archived" banner). Don't
treat them as current reference.

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
    datalib_step/  `datalib-step`: the built-in step commands —
                   download/render <source_type>, grid_index, qmd_index.
    etl/           shared ingest machinery (raw stores, blob CAS,
                   render cursors) + etl/providers/<p>/ crates, each
                   with src/download/ and src/render/ and a sibling
                   <p>_config/ crate for its config schema.
                   Three of them scan local trees and share
                   etl/src/fswalk.rs (blake3 + Unison's rescan cursor):
                   fsindex (path-keyed, no render), pdf and media (both
                   content-keyed; media has no render side either).
    migrate_config/ `datalib-migrate-config`: one-shot conversion of a
                   pre-TOML `config.yaml`. Holds every retired config
                   schema and the tree's last YAML parser, so the
                   shipping programs accept only `config.toml`.
    core/          data-root layout, the feedback + job stores,
                   host-runtime helpers. Knows nothing about the index.
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
               dev/archived/ historical plans.
third-party/   vendored upstream code (see below).
```

## The sync pipeline in one paragraph

`datalib-dag <config.toml>` runs a DAG of subprocess steps declared as
the config's `[[steps]]` tables; edges are derived from output/input path
overlap, never written by hand. Each source is a `<name>.download` +
`<name>.render` step pair (`datalib-step download|render <type>`), and
two shared fan-in steps index every source's `rendered_md` tree:
`grid_index` (SQL index at `unified_index/grid/db.doltlite_db`) and
`qmd_index` (semantic search at `unified_index/qmd/`). Both are read by
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
Pre-TOML `config.yaml` files (both the YAML steps shape and the retired
`sources:` one) are converted out of band by `datalib-migrate-config`,
the only place their schemas — and the tree's last YAML parser — still
live. Any executable
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
in `datalib/backend/unified_index/src/qmd/mod.rs`): the Tauri app
bundles a pinned Node runtime plus registry-installed `latchkey`/`qmd`
trees (staged by
`datalib/tauri/stage-runtime.sh`, resolved by
`datalib_core::node_runtime`), and every other environment — and
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

## The grid_rows union table

The Vue grid is backed by a single denormalized table, `grid_rows`,
populated by the `grid_index` step from every provider's
`*.grid_rows.json` sidecars. The Rust backend
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
3. Update the row mapper in `datalib/backend/core/src/dolt_repo.rs`
   to read it back, plus `SearchRow` in `search.rs` if the column reaches
   the API.
4. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

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

The backend opens the data root's `backend_index.doltlite_db` via
`sqlx::sqlite::SqlitePool` and wraps it in `DoltRepo`
(`datalib/backend/core/src/dolt_repo.rs`). doltlite is statically
linked into every Rust binary by `//third-party/doltlite:sqlite3`
(see `MODULE.bazel`); no host `dolt` install, no subprocess, no MySQL
client. The same pool serves reads and writes.

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

Use the Bazel-built shell, which links the same amalgamation the Rust
binaries do:

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
<data_root>/<name>/raw/entities.doltlite_db      per-source entities + sync bookkeeping
<data_root>/<name>/raw/blobs.doltlite_db        content-addressed blobs
<data_root>/unified_index/grid/db.doltlite_db   grid_rows / markdowns / edges
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

There is also a host `/usr/local/bin/doltlite` on some machines. Prefer
the Bazel target: it is version-locked to `MODULE.bazel`'s pin, so it
can't silently disagree with what the pipeline wrote.

**From a test**, take it as a `data` dep and pass `$(rootpath ...)`
rather than shelling out to a host binary — that keeps the test
hermetic. `//tests/fixtures:ingested_tng_test` is the worked example:
it opens the stores the pipeline just wrote and asserts row counts and
per-provider coverage. Prefer that over grepping tracing events out of
stderr; a log line tells you what the code *said*, the store tells you
what it *did*.

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

**Default to `bazelisk test //...` for any "are tests passing?" question.**
It's the source of truth: it runs Rust, cross-language goldens, and the
Playwright e2e suite in one shot, the same way CI does. Bazel's action
cache makes re-runs cheap — unchanged targets are served from cache, so
iterating costs only what you actually touched. For a tight inner loop,
narrow the *bazel* invocation to the package you're touching
(`bazelisk test //datalib/backend/etl/...`) — don't shell out to
`cargo` / `pnpm`, which bypass the cache and can disagree with CI.

**Do not add `--test_tag_filters=-manual,-external` to this invocation.**
The canonical line is the bare `bazelisk test //...`. Filtering on
`-external` silently drops `//datalib/ui:e2e_test` (Playwright), which
lets UI regressions through. (The lint/typecheck gate — `//:lint`, i.e.
ruff + pyright + vue-tsc — is fully hermetic and carries no tags, so no
filter can drop it; clippy and fmt ride the always-on rustfmt aspect and
always-on clippy aspect.) If a test is host- or
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
bazel run //datalib/backend/etl/providers/anthropic:anthropic_live.update
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
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl:datalib_etl))'   # 67 test targets
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
types, each its own stanza/step pair. The API downloader normalizes
every response into the bulk-export on-disk shape
(`normalize_to_export_shape` in
`datalib/backend/etl/providers/anthropic/src/download/normalize.rs`,
stamping `_source: { via: "claude.ai/api", org_uuid }` provenance) so a
single render path consumes either source indistinguishably. See
`datalib/backend/etl/providers/anthropic/DOWNLOAD.md`.

## Unordered collections: give a bag an order before storing it

**A JSON array is not necessarily a list.** When an API returns a *set*
— capabilities, permissions, tags, member ids, labels — the array order
is whatever the server happened to emit, and nothing promises it is
stable between fetches. Sort it before it goes into a content payload.

Left unsorted, a re-fetch of an unchanged object serializes differently
from itself, and everything downstream believes it changed:
`dolt_diff_<table>` reports a modification, the entity re-renders, and
the manual-e2e golden's `--reset-and-redownload` stability check fails
on content that never moved. Found this way on 2026-08-31 — claude.ai
returns a project's eight `permissions` strings in a different order on
different fetches (`canonicalize_project_payload` in the anthropic
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

## Fallbacks: prefer failing loudly to succeeding quietly

**Avoid fallbacks.** The dangerous ones *succeed*: a correct answer
reached the slow or lossy way raises no error, so an assumption that
expired weeks ago hides behind a vague "feels slow". If you add one
anyway, log when it fires. Worked example: #225 — the DAG runner spent
40s hashing 3.4GB to version a step it had already skipped, on every
run, for two weeks, before anyone noticed.

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
