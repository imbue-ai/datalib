# datalib — agent runbook

Quick references for AI/human contributors working **on the datalib
codebase**: where the docs are, how the repo is laid out, and the
conventions that aren't obvious from the code. If you are an agent
*using* datalib (running syncs, querying a user's mirror, writing a
custom step), start with [`agent_user.md`](docs/agent_user.md) instead.

## Doc map

Where to start for each area. Only the docs directly under `docs/dev/`
and the READMEs beside the code describe the tree as it is; those are
what this map lists. Three directories hold records instead, and each
file there opens with a banner saying what it is and how current:

- [`docs/dev/plans/`](docs/dev/plans/) — intended work, some of it
  partly built. `head -n 8 docs/dev/plans/*.md` reads every banner.
- [`docs/dev/plans/completed/`](docs/dev/plans/completed/) — landed
  plans, kept as the record of what was decided. A landed plan moves
  here, or is rewritten as reference under `docs/dev/` if someone would
  read it to learn how the system works; when it stops being worth
  keeping, **delete it** — git has it.
- [`docs/dev/audits/`](docs/dev/audits/) — dated reads of the tree
  against the rules, with what was fixed and what is still open.

**Don't add a plan, a completed plan or an audit to this map**, and
don't add a line for a new reference doc unless it is where someone
starts on an area. "Read X before touching Y" goes in Y's header, where
the person touching Y will see it. A list every PR appends to is a
merge conflict waiting to happen.

**Pipeline / sync engine**

- [`datalib/backend/dag/README.md`](datalib/backend/dag/README.md) — the runner's rules: graph, staleness, versions, diagnostics, locks, progress. **Start here.**
- [`docs/dev/step_protocol.md`](docs/dev/step_protocol.md) — how to write a custom step command; `datalib-step` is the reference implementation.
- [`docs/dev/config_model.md`](docs/dev/config_model.md) — what a config is made of: groups, steps as `(group, function)`, ingest methods and their reach, the fan-ins' `inputs`. [`configs/dag_example.toml`](configs/dag_example.toml) is a complete, commented one.
- [`docs/dev/logging.md`](docs/dev/logging.md) — the one log store, who writes it, how to add a line from each writer and how to read it. Read before adding a `tracing` line, a UI event or a log endpoint.

**Data architecture**

- [`datalib/backend/etl/README.md`](datalib/backend/etl/README.md) — the shared ingest machinery: keys, sidecars, volatile fields, **the doltlite pool rules**, schema changes and the migration ladder, the blob CAS. Read before opening any store.
- [`datalib/backend/etl/macros/README.md`](datalib/backend/etl/macros/README.md) — the four table derives.
- [`docs/dev/data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md), [`…_practices.md`](docs/dev/data_architecture_ingestion_practices.md) — download: principles, then how to build a provider. A provider's own quirks are in the `INGEST.md` beside its code, where it has one.
- [`docs/dev/data_architecture_parse_and_render.md`](docs/dev/data_architecture_parse_and_render.md) — render: projection to `GridRow` + markdown, incrementality.
- [`docs/dev/latchkey.md`](docs/dev/latchkey.md) — how a web source signs in: the three kinds of latchkey service, who names an account, the keychain, the gateway. Read before touching a sign-in.
- [`docs/dev/email_download_modes.md`](docs/dev/email_download_modes.md) — JMAP, Gmail API, mbox.
- [`docs/dev/grid_rows.md`](docs/dev/grid_rows.md) — the `grid_rows` union table and how to add a column.
- [`docs/dev/contacts.md`](docs/dev/contacts.md) — who a handle is: handles, each source's record of a person (`NormalizedContact`), the contacts app, and the chips that draw them. **Start here** for anything about a person; read before adding a handle kind.
- [`docs/dev/edges.md`](docs/dev/edges.md), [`docs/dev/entity_ids.md`](docs/dev/entity_ids.md) — cross-document edges; the one rule for minting a uuid (read before any `*_uuid` recipe).
- [`docs/dev/doltlite.md`](docs/dev/doltlite.md) — what the engine does (branches, locks, reads, diffs, plans, write cost, gc), inspecting `.doltlite_db` files, exporting to plain SQLite; tutorial in [`doltlite_codelab.md`](docs/dev/doltlite_codelab.md).
- [`docs/dev/app_stores.md`](docs/dev/app_stores.md) — the stores `datalib-http` owns and where every store lives under a data root.

**UI**

- [`docs/dev/cards.md`](docs/dev/cards.md), [`docs/dev/dactal.md`](docs/dev/dactal.md) — the card system and the containers layout that hosts every card; the dactal view bridge.
- [`docs/dev/chips.md`](docs/dev/chips.md) — a person, a group or a step drawn inline: the link a chip is written as, the resolvers, the clicks. Start here to add a kind of chip or a place that draws them.
- [`datalib/backend/etl/chat-common/README.md`](datalib/backend/etl/chat-common/README.md) — the one chat layout and the sanitizer allowlist. Read before changing how a message looks.
- [`docs/dev/wizard_design.md`](docs/dev/wizard_design.md) — how an "Add source" form is put together: what is basic, what is advanced, how each part is worded. Read before adding or changing a catalog entry.
- [`docs/dev/wizard_file_pickers.md`](docs/dev/wizard_file_pickers.md) — a path field in the source wizard offers a native picker.
- [`docs/dev/applets.md`](docs/dev/applets.md) — how to write an applet, and the secret every applet requires.

**Dev workflow**

- [`docs/dev/first_time_dev.md`](docs/dev/first_time_dev.md) — build and run from source.
- [`docs/dev/style.md`](docs/dev/style.md) — how code is shaped: functional core, imperative shell; how to audit the docs and the code for drift and copies.
- [`docs/dev/testing.md`](docs/dev/testing.md) — the test suites, insta `.update` targets; [`coverage.md`](docs/dev/coverage.md). Writing or fixing a Playwright spec: read its §"Writing a spec that does not flake" first.
- [`docs/dev/ci.md`](docs/dev/ci.md) — CI, its caches and BuildBuddy, and reading a run.
- [`docs/dev/release_steps.md`](docs/dev/release_steps.md) — how a release is assembled, and testing its steps from a mac.
- [`docs/dev/curl_impersonate.md`](docs/dev/curl_impersonate.md), [`runtime_fetch.md`](docs/dev/runtime_fetch.md), [`docker.md`](docs/dev/docker.md) — what ships beside the binaries: the Chrome-impersonating curl, the Node runtime, the container image.
- [`docs/dev/qmd_behaviour.md`](docs/dev/qmd_behaviour.md), [`qmd_vendored.md`](docs/dev/qmd_vendored.md) — measured facts about qmd; `third-party/qmd` is a reference snapshot, not what we run.
- [`docs/dev/history.md`](docs/dev/history.md) — facts about the tree git cannot tell you. Add a paragraph when you learn one.

**User-facing**

- [`docs/user/first_time_user.md`](docs/user/first_time_user.md), [`docs/user/getting_your_data.md`](docs/user/getting_your_data.md), [`docs/user/config_examples/`](docs/user/config_examples/).

## Keep a forward path for existing data

**A data root that works today should still work after an upgrade.**
datalib is alpha and we don't promise stable bytes at rest yet, but we
are aiming to, so treat every existing store and config as something
the next build has to carry forward. A raw store may hold what upstream
has since deleted; re-downloading is not a free undo.

When you find a name that lies or a shape that fights you, still fix
it properly — then bring the existing data along:

- **A raw store whose shape changes** gets a rung on its migration
  ladder (`datalib/backend/etl/README.md` §"The migration ladder"),
  which the launch's migrate pass climbs
  (`datalib/backend/dag/README.md` §"Upgrading a root"). The raw shape
  every release left is kept in `datalib_step/raw_shapes/`, and a test
  migrates each one.
- **A config shape that stops loading** gets a rewrite: in
  `datalib-http`'s config upgrade when it can be made without asking,
  otherwise in `datalib-migrate-config` (`docs/dev/config_model.md`
  §"The retired shapes").
- **A derived store** (a render store, the grid or qmd index) can be
  rebuilt from the raw stores; a change that costs a re-render or
  re-index is fine.

Where no migration can be written, a reset (`datalib-dag --reset
<step>`) is the last resort; say so, and why, in the commit message.
Keep a compatibility path, too, where the input comes from a
**person** rather than from our own code — a filter somebody typed
into the search bar lives in their fingers and in their saved queries,
and an alias costs one line.

## A change keeps the docs true

The reference docs — the ones the doc map lists, and the `README.md`,
`INGEST.md` and `TRANSLATE.md` files beside the code — say what the
tree does now. **A change that makes one of them wrong fixes it in the
same change.** A doc carries no "current as of" date and no "the code
wins" hedge: it is either right or it gets fixed. How to audit them
against the tree, and the tree for copies:
[`style.md`](docs/dev/style.md) §"Auditing the tree for drift and
repetition".

Plans, audits and commit messages are records of the day they were
written, not of the tree. Before repeating a "we now do X" or "X still
needs doing" from one of them as current fact, check it:

```sh
git show --stat <sha>                    # did that commit touch what its message says?
git log --diff-filter=A -- <path>        # was this file ever actually added?
grep -rn <thing-said-to-exist> <subtree> # is the thing there at all?
```

**Test-quality claims are the highest-risk category**, because a false
one is self-concealing. Treat "now covered by a test" as unverified until
you have read the assertion — and for a test whose job is to catch a
silent no-op, until you have watched it fail against the broken behavior.

## Write plainspoken

In docs — and in the few comments you keep — be clear and unhurried,
explain a term the first time it appears, and don't assume the reader
already shares your context. Plainspoken means *clear*, not *long*.

## Functional core, imperative shell

**Compute a decision as a pure function over values; act on it in a
thin layer that does nothing else.** What that means here, the
templates already in the tree, where to split and the one exception:
[`docs/dev/style.md`](docs/dev/style.md).

## Comments: few, short, and about *why*

**Write the code as if comments did not exist.** A comment is the fallback
for what you could not say in a name or a shape. Reach for a better name,
a smaller function, or a named intermediate variable first.

Keep: a **file header** (one to three sentences: what this file is for,
what belongs here); a **type header** where a struct or trait is not
self-evident; a **why the code cannot carry** — a non-obvious constraint,
a workaround for someone else's bug, a trap the next person will fall
into. State the rule rather than narrating how we arrived at it.

Delete on sight: **function-level doc blocks** (rename or split instead);
**restatement** (`// increment the counter`); **changelog** ("used to",
"before #209", "as of 2026-08-31" — git knows, and a comment that dates
itself is a comment that will be wrong); **essays** (if it is worth
several paragraphs it is a document — put it in a `README.md` beside the
code and link it in one line).

Tests are the one place a short doc comment on a function earns its
keep: a sentence or two naming the regression it guards.

Every comment is a claim that has to be re-verified on every edit. Fewer,
truer comments beat more of them.

## Repo layout

```
datalib/
  backend/     Rust workspace.
    dag/           `datalib-dag`: the DAG runner. `//datalib/backend:bin`
                   stages every shipped binary under its public
                   `datalib-*` name in one directory (`:dist`); build
                   that whenever you need to actually run a pipeline.
    datalib_step/  `datalib-step`: the built-in step program. A step
                   with no `command` runs it; it reads its function and
                   its group's type from the environment.
    etl/           shared ingest machinery (raw stores, blob CAS, render
                   cursors) — the download side, and where a
                   downloader's dependencies stop.
    etl/files/     `datalib_etl_files`: what changed on disk for a
                   source that reads local files (fsscan, the
                   fingerprint cache, the per-feed file checkpoint).
                   Only those sources link it.
    etl/web/       `datalib_etl_web`: what a source that reaches a web
                   service shares — `latchkey curl` with retries and
                   stops, HTTP playback, DAV, the owed-record
                   bookkeeping. Only those sources link it.
    etl/render/    `datalib_etl_render`: the render store, the
                   unified-index load, `RenderCtx`. Everything that knows
                   `datalib_schema` sits here or above.
    etl/timeseries_render/ what the time-series render crates share.
    etl/providers/ <p>/ (ingest) + <p>_render/ (render) + <p>_config/
                   (config schema) per provider. Twelve of the
                   file-backed ones scan a local tree through
                   etl/files/src/fsscan.rs (claude_code and codex by way
                   of etl/agent_sessions/; fsindex has its own walker
                   over etl/files/src/fswalk.rs); four mirror a SQLite
                   file through etl/sqlite_mirror/; three render time
                   series (airvisual, yolink, garmin). fsindex, media,
                   lightroom and apple_photos have no <p>_render.
    etl/sqlite_mirror/ the table-for-table SQLite→doltlite mirror engine.
    table/         `BulkUpsertable`, alone.
    probe/         what "Check connection" and a picker's "Load" ask
                   and answer, alone.
    migrate_config/ `datalib-migrate-config`: rewrites the one retired
                   config shape into the current one.
    runtime/       the data-root layout, which build this is
                   (`build_id`), the bundled-Node resolver (the `npx`
                   fallback is opt-in and loud) and the qmd model
                   pins. No dependencies, so anything can link it.
    qmd_indexer/   `Index`: the qmd index's operations — register the
                   collections, keyword-index or embed one source — over
                   qmd's SDK. Tested against the real qmd.
    qmd_models/    puts qmd's pinned GGUFs in place, sha256-verified,
                   so qmd never fetches one itself. Linked by the step
                   and the applet.
    store_meta/    `_datalib_meta`, the table every store carries naming
                   the build that wrote it and the shape it is in.
    core/          the app stores plus re-exports of `runtime`.
    query/         the search-bar grammar every grid shares; no deps.
    handle/        `Handle`: one identifier for a person (`email:`, `tel:`,
                   `slack:`), normalized; what renders write as
                   `data-handle`. No first-party deps.
    contact_schema/ `NormalizedContact`: a person as one source describes
                   them. Only the shape; contact-common renders it.
    contacts/      the contacts app's store under `datalib_curated/`;
                   the `datalib_contacts` applet is its one writer.
    unified_index/ the grid index, the qmd index, the query language over
                   them. Linked by datalib-step and datalib-applet —
                   never by datalib-http or datalib-dag.
    applets/       `datalib-applet`: the applet host.
    history/       a doltlite store's commit log; its one first-party dep
                   is `pin`, so datalib-http can serve it without
                   linking `etl`.
    http/          `datalib-http`: API server + sync loop + UI host +
                   applet gateway. Every route is behind a per-process
                   API token (src/auth.rs): read
                   `<root>/system/api-token`, send
                   `Authorization: Bearer <token>`.
    schema/        `grid_rows`/`edges`/`markdowns` row structs;
    app_schema/    feedback, disk usage, remote media, runs; both derive
                   DDL via `#[derive(PortableTable)]`.
  ui/          Vue frontend; every grid is SlickGrid, kept behind a few
               files so it can be swapped (docs/dev/cards.md § The grid).
  tauri/       the desktop shell (out of Bazel).
tests/fixtures/  the TNG fixture pipeline: it syncs the providers' TNG
               source data (each kept in its provider's tests/fixtures/)
               into the cached `ingested/` artifact.
docs/          dev/ architecture notes; user/ guides; dev/plans/; assets/ images
               only the docs use (the README grid shares the UI's marks).
third-party/   vendored upstream code.
```

A provider's config schema is its own crate (`<p>_config`, serde
structs and nothing else) so anything that needs to *understand* a
config can link it without the machinery. The `<p>_render` split is the same move (see §"Ingest and render are
separate crates").

## The sync pipeline

`datalib-dag <config.toml>` runs a DAG of subprocess steps. A `[[groups]]`
entry is one thing on the Manage screen (a source is a group with a
`type`; the unified index is a group without one); a `[[steps]]` entry is
`group` + `function`, its id composed as `<group>/<function>` — the one
tree it writes; `inputs` name steps by that id and are the edges; an
`[[applets]]` entry is a server the gateway spawns. A built-in step has
no `command` and runs `datalib-step`.

Each source has an `ingest` step and (most) a `render_markdown` step, and
`grid_index` under `unified_index` reads every render tree its `inputs`
name into the SQL index the grid reads. A searched source fills its own
collection of the qmd index (free-text search) with two more steps of
its own, `keyword_index` and then `embed`, so the slow embedding can be
turned off or run by hand per source. `qmd_aggregator` reads every
source's pair: it retires the collection of any source it does not name
and reports on the whole, and removing it turns search off.
`embedding_map` reads the aggregator and lays the embeddings out on a
plane for the map card (`datalib/backend/embedding_map/README.md`).
All of it is queried by the `unified_index` applet; `datalib-http`
reads only the stores' commit logs (`history/`), never their rows. A render
store is readable at every commit: the documents between two checkpoints
share one transaction. The loop's record — each step's state now, its
last run and success, each sink's version — is in
`system/supervisor.sqlite`, and a Manage row's Status is that state. A
config entry the loader cannot use costs that entry and nothing else;
`datalib-dag --check <config>` says what went and why. A config the app
cannot serve anything from comes back as `app_ready: false` and the UI
shows `ConfigErrorView`, live in both directions. The http server runs
the loop `datalib-dag` runs, in-process (`http/src/supervisor.rs`), holding
`runner-lock` for its life; a sync, a stop, a step turned off is a row it writes
there (`POST /api/requests`, `/api/steps/<id>/turn_off`); the Manage tab
edits the config; a root with no config gets the launcher and the
first-run screen.

## Ingest and render are separate crates

A provider is three crates: `datalib_etl_<p>_config`, `datalib_etl_<p>`
(fetches), and `datalib_etl_<p>_render` (markdown + `grid_rows`). The
framework splits the same way — `datalib_etl` below, `datalib_etl_render`
above. **The render schema stops at that line:** `datalib_schema` is
reachable from render crates and from nothing on the ingest side.
Nothing in the build refuses an ingest crate that takes it; the
measurement below is what notices.

- **Anything an ingest needs lives on the ingest side.** The uuid recipes
  are minted during download and read again during render, so they
  belong in `ingest/schema_raw.rs` and the render crate names them
  through the download crate.
- **A source that renders nothing has no `_render` crate.** `ingest_only!`
  in `datalib_step/src/dispatch.rs` says so once, and
  `SourceType::item_table` names the raw table whose rows are its
  Items count, since it has no documents to count them.

The measurement that checks it:

```sh
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

73 at the last count. If that number climbs, something took a dependency
it should not have.

## The grid_rows union table

The grid is one denormalized table, `grid_rows` (the `GridRow` struct in
`datalib/backend/schema/src/grid_rows.rs`), which `grid_index` fills
from every source's render store and the `unified_index` applet reads
with one SELECT — no per-provider branches in the query path.
`docs/dev/grid_rows.md` has the checklist for adding a column.

## QMDs are write-only

The render step emits markdown files; the backend serves them
**verbatim** and never parses them back. Structured fields come from
`grid_rows`. Per-section anchors come from the
`<div id="m-{uuid}" data-section-uuid="{uuid}" class="msg …">` wrappers
the renderer emits — those two attributes are load-bearing
(`ui/src/feedback/context.ts::messageAncestor`). A new renderer needs
that pair and nothing else. If you find yourself writing a QMD parser in
the backend, stop — add the field to `grid_rows` instead.

## Doltlite: one writer per file, readers pinned

**Stock `sqlite3` cannot open a `.doltlite_db`**; it is a prolly-tree
store only a doltlite-linked binary reads. Any store turns into a plain
SQLite file with one pipe (say this whenever you say the warning):

```sh
datalib-doltlite -readonly <store>.doltlite_db .dump | sqlite3 out.sqlite
```

In a checkout use `bazelisk build //third-party/doltlite:doltlite`, a
sqlite3-shell drop-in version-locked to the tree; from a test take it as
a `data` dep. [`docs/dev/doltlite.md`](docs/dev/doltlite.md) is the one
place for what the engine does, with the recipes;
`docs/dev/app_stores.md` maps which store lives where and who owns it.
Every fact there is a test in `//datalib/backend/doltlite_facts:doltlite_facts_test`
or `//datalib/backend/etl:doltlite_two_process_test`, so a doltlite bump
that moves one fails by name.

The rules, none optional. How our code enforces them is in
`datalib/backend/etl/README.md` §"Connection pools":

- **One writer per file.** Doltlite keeps uncommitted rows per branch
  *in the file*, so two writers on one branch commit each other's
  half-written rows, and doltlite itself only makes a second writer
  wait its turn, never refuses it
  ([locks and writers](docs/dev/doltlite.md#locks-and-writers)). Our
  per-file lock refuses it. The `grid_index` step owns the index;
  `datalib-http` owns feedback, usage and remote media; the applet only
  reads. A download takes its store as an input
  (`FetchOptions.db: RawDb`) and never opens one.
- **A writer works on `datalib_writer`, never on `main`**, and
  fast-forwards `main` when it seals, so a reader never sees a
  half-written batch or a half-built schema. `commit_run` is the seal —
  a bare `dolt_commit` publishes nothing and reaches no reader. `main`
  moves only by that force-move, which is right only while one process
  moves it.
- **Every pool is `max_connections(1)`** with recycling off, and there is
  one open per file per pass. `close().await` before the next open, on
  the error path too — dropping the handle only schedules the close.
- **A reader opens read-only and reads one commit**, one of
  [three ways](docs/dev/doltlite.md#three-ways-to-read-one-commit):
  `dolt_at_<t>('<hash>')`, a held read transaction, or a detached
  read-only open of `<file>@<hash>`. Render and `grid_index` use the
  last (`doltlite_raw::open_reader`), so their queries name the tables
  themselves; the search applet holds a transaction
  (`DoltRepo::pinned`). A process that *moves a ref*, even on its own
  branch, is a writer.
- **A statement a reader adds is presumed guilty until
  `doltlite_two_process_test` has run with it.** Looking like a read is
  not enough; `etl/README.md` has the allowlist.
- **Never run a store call on a runtime you are about to drop**;
  `indexed_markdown::blocking` keeps one process-wide runtime for that.

**Expect this to pass locally and fail on CI.** Overlapping pools collide
by timing and filesystem locking, and a mac laptop and a Linux container
disagree readily. If a doltlite-touching change is green locally and red
or slow in CI, count the opens first.

## License: MIT, and what may come in

The repo is MIT (`LICENSE`). Every dependency is gated by
`datalib/backend/deny.toml`'s allow list (permissive licenses only), and
a release ships the notices of everything it bundles — Rust crates, the
UI bundle, DoltLite, curl-impersonate, Node — assembled by
`scripts/third_party_notices.sh`. Two rules for code that is not a
dependency:

- **Nothing copyleft gets vendored or ported**, however small. A GPL or
  AGPL project may be a test oracle or a source of facts about a wire
  format (`whatsapp-backup/src/key.rs`, `signal-backup/proto/`), never
  a source of code. The one exception is an image in a test fixture
  (the TNG contacts' photos), which may be CC BY-SA beside the
  permissive ones, credited where it is used.
- **Say where it came from.** Ported MIT/BSD code names the project and
  its copyright line in the file header. Vendored code keeps its
  `LICENSE` and a README naming the upstream commit
  (`third-party/latchkey-garmin/`).

## Git: prefer merges over rebases

`git pull` (default merge), not `git pull --rebase`. Rebasing rewrites
local hashes and loses what actually happened; force-push is off the
table on shared branches. A PR lands as a merge commit too (next
section), so never squash a branch yourself: `git reset --soft` onto a
`main` that has moved since the branch was cut commits a tree that
silently reverts everything landed in between (#745 undid #743 that
way).

**`MODULE.bazel.lock` is never resolved by hand.** It is generated, and
two branches that both moved it conflict textually even though the
right result is always "regenerate on the merged tree". `.gitattributes`
marks it `merge=union` so the merge itself goes through; then any
`bazelisk build` rewrites it and `//:lint_repo` refuses a stale one.
So the resolution is: merge, build, commit. GitHub's merge button knows
nothing of `.gitattributes` and still reports a conflict when two open
PRs both touched it — the second one merges main and pushes.

## Push early, open the PR early, watch CI, and turn on autofix

Push the branch and open a PR as soon as there is something to test —
CI's runners are free — as long as nothing in it is private data
(§"Real data stays out of the repo"). Then turn on autofix for the PR,
so a red run, a merge conflict or a review comment comes back to the
agent that wrote it instead of waiting for a person. After pushing,
check `gh pr view <n> --json mergeable,mergeStateStatus` and follow the
run to its end. If it fails, read the failure; if the failed target
looks like a flake (the doltlite-timing ones, or anything
`scripts/flaky_tests.py` lists), re-run the failed jobs once before
digging in. Before pushing a follow-up, confirm the PR is still open —
a merged PR does not reopen.

**A PR lands as a merge commit**: `gh pr merge <n> --merge`, or
"Create a merge commit" on GitHub — not squash, not rebase. The
branch's commits then stay ancestors of `main`, so git can say whether
a branch landed (`git merge-base --is-ancestor <branch> origin/main`),
a branch cut from another unmerged branch merges cleanly once that one
lands, and a follow-up after a merge is a new PR from the same branch
carrying only the new commits.

**Read `main` along its first parents**, which is one merge commit per
PR, titled after the PR:

```sh
git log --first-parent origin/main
git bisect start --first-parent      # steps PR by PR, never mid-branch
git blame --first-parent <path>      # which PR, not which fixup
```

Plain `git log` and GitHub's commit list also show every commit on
every branch; that is the price. The merge commit's subject is the PR
title, so write the title as the line you want in that log.

## Python deps: pyproject.toml → requirements.txt → Bazel

`uv run` reads `pyproject.toml` + `uv.lock`; Bazel's `pip.parse` reads
`requirements.txt`, a generated file. After any `pyproject.toml` change:

```sh
uv export --no-emit-project --no-emit-workspace --format requirements-txt -o requirements.txt
```

then add `requirement("newpkg")` to the relevant `BUILD.bazel`. Python is
only used for fixture / test tooling and scripts; everything shipping is
Rust.

## Running tests

**"Build green" means `bazelisk test //...` passes — nothing less.** A
narrower invocation is fine for the inner loop, but don't call the tree
green based on one; say what you actually ran. The CI gate is that plus
the repo hygiene lint, which runs first and skips the tests if it fails:

```bash
bazelisk run //:precommit          # the one command to run before pushing
```

That is `//:lint_repo`, `//:lint`, a `bazelisk build //...` (which runs
the rustfmt and clippy aspects over every crate) and every hermetic
test. Measured on one warm mac:

| loop | command | cost |
|---|---|---|
| lint + typecheck | `bazelisk test //:lint` | ~3s |
| every hermetic test | `bazelisk test //... --build_tests_only --test_tag_filters=-no-sandbox,-requires-network,-external,-manual` | ~106s after a shared-crate edit, ~2s when nothing moved |
| the package you're editing | `bazelisk test //datalib/backend/etl/...` | varies |
| the whole gate, e2e included | push, and read CI | ~3 min warm / ~20 min cold |

The filtered line skips formatting: with `--build_tests_only` the
aspects run only on the tests named, not the libraries they link.
**Those tag filters belong on that line and nowhere else** — on the full
run, `-external` silently drops the Playwright suite.

**Bazel is the only supported driver.** `cargo test` / `pnpm test` bypass
its cache and sandbox and can disagree with CI. Anything that reads
`bazel-bin/tests/fixtures/ingested/*` is reading a genrule output; go
through bazel so the fixture is rebuilt first. Insta snapshots are
updated through sibling `.update` targets (`docs/dev/testing.md`).
Coverage: `docs/dev/coverage.md`. Why a run was slow: `docs/dev/ci.md`.

## Tests wait on the observable, never on the clock

**A test never `sleep`s to order two writers, or to give something
time to happen; it waits for the thing it is about to assert.** A
sleep that is long enough on a warm mac is short on a loaded CI runner
(the one on `2cbcc398` was), and a sleep that is long enough on CI
makes every local run slower than it needs to be. Poll the row, the
file, the endpoint — with a deadline, so a hang is a failure that
names what never arrived rather than a timeout with no message. In the
Playwright suite, where most of our flakes have been, the rules are in
[`testing.md`](docs/dev/testing.md) §"Writing a spec that does not flake".

Three neighbours of the same mistake:

- **A fixed timestamp in a test is a bomb** wherever anything is
  measured from `now` — a retention window, a "recent" filter. Either
  derive the stamp from `now`, or set the window in the test so wide
  that the calendar cannot reach it (`process_log_days: 36500`, #567),
  and say which in a comment.
- **A test binary runs its tests as threads of one process**, so
  anything keyed on the process is shared between them. A temp path
  built from `std::process::id()` is the common case: two tests get the
  same file and one's cleanup deletes the other's. Take a
  `tempfile::tempdir()`; in code that cannot reach for a dependency,
  add a process-local counter to the pid. An environment variable is
  the same trap — one test's `set_var` is every test's, and outlives
  it. Prefer passing the value in (`models_dir_under` in
  `qmd_indexer`); where the variable itself is what's under test, take
  a lock and restore on drop (`EnvGuard` in `node_runtime.rs`).
- **A test that takes more than a third of its timeout on CI is a
  flake waiting to happen** once the runner is busy. Tag it `cpu:N`
  or `exclusive` so bazel schedules it alone, or raise the timeout and
  say why. `scripts/flaky_tests.py` names the ones that have already
  flaked; a target it lists twice needs one of those two fixes, not a
  re-run.

## Real data stays out of the repo

**Nothing from a person's mirror goes into the tree or onto GitHub**: not
a fixture, snapshot, test string or comment, and not a commit message, PR
description or issue. The repo is public, and a force-pushed commit stays
reachable by its hash. Learn a shape from a real root, then write the test
in made-up TNG data; counts, sizes and timings are fine to quote, what the
records say is not.

## Common commands

```bash
bazelisk test //...                                   # source of truth
bazelisk test //datalib/backend/...                   # narrower, still bazel
bazelisk build //tests/fixtures:ingested_tng          # rebuild the fixture ingest
bazelisk build //datalib/backend:bin                  # stage every shipped binary
bazel-bin/datalib/backend/bin/datalib-dag <data_root>/config.toml
```

## "Claude", not "Anthropic"

**Every source type is named for the product a person recognizes, never
for the vendor or the way it is reached**: `claude` (api or export),
`chatgpt` not `openai`, `contacts` whether CardDAV or `.vcf`. Write
**Anthropic** only where you mean the company or something it issues (an
org in Anthropic's account system, the UUIDs it mints). The one
survivor is the `anthropic` search keyword in `ui/src/config/catalog.ts`.

## A source's id is not its name

| | |
|---|---|
| **id** | its group id — the directory under the data root, the stem of its step ids. Path-safe, unique; changing it is a migration. |
| **name** | what a person typed in the wizard. Free text, mutable, may repeat. |

Everything that identifies, filters or joins uses the id, and the field
is `source_id` everywhere, the `source_id:` search filter included.

## A local file or folder: ask `fsscan` what changed

**Don't walk a folder or re-read an input yourself to learn whether it
changed; ask `datalib_etl_files::fsscan`.** It hashes each file once per host
(a shared fingerprint cache), so an unchanged input costs a `stat` —
milliseconds, where re-reading costs seconds — and `file_checkpoint`
keeps this source's `path → blake3` cursor to diff against. The recipe
is `datalib/backend/etl/files/README.md` §"Answering "did it change?"
for a file-backed source"; `lightroom`'s `ingest/sync.rs` is a small example.

## A network source owes what upstream listed and the store does not hold

**Never store a position in a walk, and never mark a record done.** A
download stores what upstream *listed* (key and version), the version
each record's content satisfies (`held_version` on its `_bookkeeping`
sidecar, written with the content), and for a range, the spans already
walked (`datalib_etl_web::coverage`). What is owed is a query over those;
`datalib_etl_web::owed` fetches it and records every outcome. A stored
cursor is the bug this replaces: a run that stopped halfway, or a
config widened later, leaves work no cursor will ever name. The one
position kept is upstream's own delta token, written with the page it
covers. `docs/dev/data_architecture_ingestion.md` § "What is left to
fetch" has the per-source table; each provider's
`tests/*/interrupt.rs` is the proof it holds.

## Unordered collections: give a bag an order before storing it

When an API returns a *set* as a JSON array — capabilities, permissions,
tags, labels — sort it by the rendered string before it goes into a
content payload. The pipeline's incrementality rests on an unchanged
record serializing identically to itself. **Sort; don't declare it
volatile**: volatile drops the field, and losing a permission is a real
change you want to see.

## Name a closed set of strings

**If a string can only be one of a handful of values, it is an enum.**
Use `strum`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(EnumString, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunState { Running, SkippedUpToDate, … }

impl RunState {
    pub fn as_str(self) -> &'static str { self.into() }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> { s.parse().ok() }
}
```

`parse` returns `Option`, never a guess — a store written by a newer
build can name a value this binary lacks, and the caller decides.
**Add a test that strum and serde agree** when a type derives both. Leave
the stored `VARCHAR` alone and **bind** the value in SQL rather than
interpolating it. Don't reach for an enum for values that come from
upstream (block types, MIME types), free-form display text
(`grid_rows.kind`), or JSON keys.

| vocabulary | type | home |
|---|---|---|
| what a step is doing in a run | `RunState` | `dag/src/run_state.rs` |
| why a step failed | `FailureKind` | `dag/src/step.rs` |
| what the run store names | `LiveState` | `runs/src/lib.rs` |
| a log line's severity and pipe | `LogLevel`, `Stream` | `app_schema/src/runs/log.rs` |
| what the loop made of a step (`steps.state`) | `StateKind` | `dag/src/supervisor/tick.rs` |
| how a sync request ended | `RequestOutcome` | `dag/src/supervisor/store.rs` |
| a browser-login attempt, and what it is doing | `ConnectState`, `ConnectPhase` | `http/src/connect.rs` |
| who names a latchkey account a browser login adds | `AccountNaming` | `http/src/connect.rs` |
| what kind of trouble a sign-in or probe ran into | `IssueKind` | `probe/src/issue.rs` (`datalib_probe`) |
| a probe the wizard polls | `ProbeState` | `http/src/probe.rs` |
| how the launch's migrate pass stands with one step | `MigrateState` | `http/src/supervisor.rs` |
| the list a picker loads | `ProbeList` | `probe/src/lib.rs` (`datalib_probe`) |
| the `grid_rows.provider` tag | `Provider` | `schema/src/providers.rs` |
| what a step could not fully do to a record | `Outcome`, `Reason`, `ScopeKind`, `Severity`, `Stage` | `problems/src/lib.rs` (`datalib_problems`) |
| a configured entry upstream does not have; a listing or phase a run could not do | `ProblemReason`, `RunProblemKind` | `etl/src/download_problems.rs` |
| how a diff group's row differs between two renders | `DiffStatus` | `schema/src/diff_status.rs` |
| which answer to free text a search asks for | `SearchTab` | `applets/src/unified_index/tabs.rs` |
| what a term is to its grid row | `SearchTermKind` | `schema/src/search_terms.rs` (`datalib_schema`) |
| a config's source type | `SourceType` | `datalib_step/src/source_type.rs` |
| whether an ingest method reaches a service or reads files | `Reach` | `source_common/src/lib.rs` |
| which of datalib's stores a file is, in its `_datalib_meta` | `StoreKind` | `store_meta/src/lib.rs` (`datalib_store_meta`) |
| what namespace a handle is in | `HandleKind` | `handle/src/lib.rs` (`datalib_handle`) |
| a contact is a person or a group; how one is reached | `ContactKind`, `Medium` | `contact_schema/src/lib.rs` (`datalib_contact_schema`) |
| how a handle was linked | `LinkedHow` | `contacts/src/lib.rs` (`datalib_contacts`) |
| what a contact's field says | `FieldKind` | `contacts/src/lib.rs` (`datalib_contacts`) |

The TypeScript side mirrors these as string-literal unions in
`datalib/ui/src/api.ts`, hand-kept — change both halves together.

## A `deps` entry you don't use is a build error

Every `deps` / `proc_macro_deps` entry under `datalib/` must be used by
the crate that names it (`.bazelrc` turns the rustc lint on per crate). A
dep used only under `#[cfg(test)]` goes on the `rust_test`, not the
library. A dep needed but never named is kept with `use <crate> as _;`.

A first-party crate has no `Cargo.toml`; its `BUILD.bazel` is the whole
truth. `datalib/backend/Cargo.toml` lists the third-party crates and
nothing else, for crate_universe, `cargo deny` and `cargo about`. A new
one goes there and into the `deps` that take it, then
`tools/repin_cargo.sh`; `//:lint_repo` refuses an entry no `BUILD.bazel`
names.

## Fallbacks: prefer failing loudly to succeeding quietly

**Avoid fallbacks.** The dangerous ones *succeed*: a correct answer
reached the slow or lossy way raises no error. If you add one anyway,
log when it fires.

**An error or a warning about a record goes through `problems`, never
only to the log.** A record a download could not fetch goes through
`record_object_attempt` / `record_object_error`. Everything else a
download could not do goes into the one `RunProblems` its `fetch` is
handed (`run_problems::collecting`): a record that failed before we had
a row for it, a configured entry upstream does not have, a listing or
phase the run could not do as a whole. Each report says what the run
covered, and only that is cleared. A record render could not fully
project goes on its document's `RenderedMarkdown::problems`, or
through `RenderCtx::report_*` when there is no document yet. The rows
travel with the data to the index, and that is where a person sees
them: the Manage row's count (the `problems{severity=…}` metrics each
step reports) and the banner above the document. A `warn!` alone
reaches nobody.

## Dynamic SQL needs `AssertSqlSafe` and a reason

sqlx 0.9 only accepts `&'static str` as a query string. Anything built at
runtime is wrapped in `sqlx::AssertSqlSafe(...)` — an assertion *you*
make — with a comment saying why it is safe. Two patterns cover almost
everything: placeholders built from a count with every value bound, and
table/column names that are `&'static str` at every callsite. Anything
from upstream data is quoted (`lightroom`'s `plan::quote_ident`) or
bound.

## Timestamp convention

**A stamp we mint goes in a column named `<x>_at_utc`, in UTC, with one
`tz_offset` column per table** holding the offset the clock was in. Text
order is then instant order. In memory and on the wire it is one
offset-bearing string (`IsoOffsetTimestamp::now_local()`; `DATALIB_DAG_NOW`
is the run-pinned now every step should prefer); the split happens at
the write (`to_utc_and_offset()`, `datalib_time::split_stamp`). JSON files
keep the single string.

**A stamp that belongs to the record stays as the source wrote it**
(`grid_rows.created_at`, `emails.received_at`): the offset is information.
Where such a column needs to sort it gets a derived UTC twin
(`created_at_utc` + `created_offset`) rather than being rewritten. If you
find yourself writing `strftime("%Y-%m-%dT%H:%M:%SZ")`, stop —
`isoformat()` in Python, `to_rfc3339()` here.

## Auth (web API)

Every web source that needs a credential signs in through latchkey,
and its requests go out as `latchkey curl`:
[`docs/dev/latchkey.md`](docs/dev/latchkey.md). A URL that carries its
own authority skips latchkey and goes out as plain `curl` through the
same HTTP layer (`HttpRequest::plain`): YoLink's signed CSV downloads,
Notion's pre-signed file links, LinkedIn's public photos.
Cloudflare-fronted hosts go through the bundled `curl-impersonate`
([`docs/dev/curl_impersonate.md`](docs/dev/curl_impersonate.md)); if
Cloudflare still 403s, the IP or user agent may be flagged — wait it
out or swap networks.
