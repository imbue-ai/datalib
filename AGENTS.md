# Datalib — agent runbook

Quick references for AI/human contributors working **on the Datalib
codebase**: where the docs are, how the repo is laid out, and the
conventions that aren't obvious from the code. If you are an agent
*using* Datalib (running syncs, querying a user's mirror, writing a
custom step), start with [`agent_user.md`](docs/agent_user.md) instead.

## Doc map

Where to start for each area. Only the docs directly under `docs/dev/`
and the READMEs beside the code describe the tree as it is; those are
what this map lists. Three directories hold records instead, and each
file there opens with a banner saying what it is and how current:

- [`docs/dev/plans/`](docs/dev/plans/) — intended work, some of it
  partly built. `head -n 8 docs/dev/plans/*.md` reads every banner.
  A plan under review may live in a Claude Doc that people comment
  on. The doc is then the source of truth, and its file here is a
  mirror whose banner links the doc: change the doc, then export the
  tab over the file, never the file alone.
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
- [`docs/dev/packages.md`](docs/dev/packages.md) — packages: custom steps and applets published at a URL, added to a library and pinned in its config.
- [`docs/dev/config_model.md`](docs/dev/config_model.md) — what a config is made of: groups, steps as an `id` and a `command`, ingest methods and their reach, the fan-ins' `inputs`. [`configs/dag_example.toml`](configs/dag_example.toml) is a complete, commented one.
- [`docs/dev/embed_on_modal.md`](docs/dev/embed_on_modal.md) — the `embed` step on Modal's GPUs: what a person writes, what leaves the machine, how the step drives the Sandboxes and what a run reports it spent. **Start here** for anything about embedding off the machine.
- [`docs/dev/logging.md`](docs/dev/logging.md) — the one log store, who writes it, how to add a line from each writer and how to read it. Read before adding a `tracing` line, a UI event or a log endpoint.

**Data architecture**

- [`datalib/backend/etl/README.md`](datalib/backend/etl/README.md) — the shared ingest machinery: keys, sidecars, volatile fields, **the doltlite pool rules**, schema changes and the migration ladder, the blob CAS. Read before opening any store.
- [`datalib/backend/store_meta/README.md`](datalib/backend/store_meta/README.md) — the shape every store records and the one check every reader makes before it reads a row. **Start here** before changing a store's tables or indexes, or reading a store.
- [`datalib/backend/etl/macros/README.md`](datalib/backend/etl/macros/README.md) — the four table derives.
- [`docs/dev/data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md), [`…_practices.md`](docs/dev/data_architecture_ingestion_practices.md) — download: principles, then how to build a provider. A provider's own quirks are in the `INGEST.md` beside its code, where it has one.
- [`docs/dev/data_architecture_parse_and_render.md`](docs/dev/data_architecture_parse_and_render.md) — render: projection to `GridRow` + markdown, incrementality.
- [`docs/dev/deleted_upstream.md`](docs/dev/deleted_upstream.md) — `when_deleted_upstream`: keeping your copy of what you deleted upstream for privacy, and how a source offers it.
- [`docs/dev/latchkey.md`](docs/dev/latchkey.md) — how a web source signs in: the three kinds of latchkey service, who names an account, the keychain, the gateway. Read before touching a sign-in.
- [`docs/dev/email_download_modes.md`](docs/dev/email_download_modes.md) — JMAP, Gmail API, mbox.
- [`docs/dev/grid_rows.md`](docs/dev/grid_rows.md) — the `grid_rows` union table and how to add a column.
- [`docs/dev/search.md`](docs/dev/search.md) — how search works: what it reads, what it does, its SQL and which index answers it, how to measure it. **Start here** before adding a search key or an index, or chasing a slow search.
- [`docs/dev/time_series.md`](docs/dev/time_series.md) — numbers over time: a source's time-series store, the chunk encoding, UCUM units, the index that points at the stores. **Start here** for anything about time series.
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
- [`tests/fixtures/README.md`](tests/fixtures/README.md) — the TNG corpus: the made-up data every source is tested on, where each fixture lives, and how they become one synced library.
- [`docs/dev/ci.md`](docs/dev/ci.md) — CI, its caches and BuildBuddy, and reading a run.
- [`docs/dev/release_steps.md`](docs/dev/release_steps.md) — how a release is assembled, and testing its steps from a mac.
- [`docs/dev/curl_impersonate.md`](docs/dev/curl_impersonate.md), [`runtime_fetch.md`](docs/dev/runtime_fetch.md), [`docker.md`](docs/dev/docker.md) — what ships beside the binaries: the Chrome-impersonating curl, the Node runtime, the container image.
- [`docs/dev/history.md`](docs/dev/history.md) — facts about the tree git cannot tell you. Add a paragraph when you learn one.
- [`docs/dev/security_posture.md`](docs/dev/security_posture.md) — who we guard against, what stops them and what does not. **Start here** before changing a secret, a release job or a safety check, or adding anything a protected library writes outside its folder.

**User-facing**

- [`docs/user/first_time_user.md`](docs/user/first_time_user.md), [`docs/user/getting_your_data.md`](docs/user/getting_your_data.md), [`docs/user/config_examples/`](docs/user/config_examples/).
- [`docs/user/searching.md`](docs/user/searching.md) — the search's query language, `from:me` and what it covers; compiled into `datalib-step help search`, so a change to the search changes it.
- [`docs/user/applets.md`](docs/user/applets.md), [`docs/user/contacts_app.md`](docs/user/contacts_app.md) — what the running app serves, and an agent's way into the contacts app; compiled in the same way, so a change to a route changes them.
- [`docs/user/rendered_documents.md`](docs/user/rendered_documents.md) — what a rendered document promises any viewer: the link a person is, the `<time>` a date is, the attachment addresses. Compiled in as `help documents`, so a change to those shapes changes it.
- [`datalib/backend/agent_docs/README.md`](datalib/backend/agent_docs/README.md) — every doc `datalib-step help` prints, how its links print, and the checks on its TOML and commands (`toml fragment`). Read before editing a doc an agent is sent to.

## Keep a forward path for existing data

**A data root that works today should still work after an upgrade.**
Datalib is alpha and we don't promise stable bytes at rest yet, but we
are aiming to, so treat every existing store and config as something
the next build has to carry forward. A raw store may hold what upstream
has since deleted; re-downloading is not a free undo.

When you find a name that lies or a shape that fights you, still fix
it properly — then bring the existing data along:

- **A raw store whose shape changes** gets a rung on its migration
  ladder (`datalib/backend/etl/README.md` §"The migration ladder"),
  which the ingest's `--prepare`, asked at every start, climbs
  (`datalib/backend/dag/README.md` §"Upgrading a root"). The raw shape
  every release left is kept in `datalib_step/raw_shapes/`, and a test
  migrates each one.
- **A config shape that stops loading** gets a rewrite: in
  `datalib-http`'s config upgrade when it can be made without asking,
  otherwise in `datalib-migrate-config` (`docs/dev/config_model.md`
  §"The retired shapes").
- **A derived store** (a render store, the grid or search index) can be
  rebuilt from the raw stores; a change that costs a re-render or
  re-index is fine.

Where no migration can be written, a reset (`datalib-pipeline reset
<data_root> <step>`) is the last resort; say so, and why, in the commit message.
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
one is self-concealing. Treat "now covered by a test" as unverified
until you have read the assertion *and* watched it fail against the
broken behaviour; where that failure cannot be staged, say so in the PR.
A **tautological** test — one whose fixture, precondition or skip
condition came from the code it checks — reads perfectly well and cannot
fail while the bug is there: assert a precondition rather than skipping
on one, and give a synthesized fixture a live test that sends its
request. **Set a test's subject up by calling what production calls, not
by repeating it**: a helper that reassembles production's setup drifts
from it, and the test then checks something nobody runs. Call it to
*arrange*; never to decide what to expect or whether to check at all.

## Write plainspoken

In docs, PR descriptions, reviews and the few comments you keep, be
clear and unhurried, and don't assume the reader already shares your
context. Plainspoken means *clear*, not *long*: every word the reader
has to translate into what actually happens is a cost, so the default
is the literal description.

**Words: say what happens.**

- **Prefer the operation itself to a higher-level word for it.** Write
  "sends SIGINT to the step's process group", not "pokes the step";
  "sets the request's `outcome` to `stopped`", not "the loop drops the
  request". A
  longer example: each time the runner starts, it runs every step whose
  program takes the `prepare` verb with `--prepare` appended to its
  command, and the program exits once it has brought what it stored to
  this build's shape. Write "runs the step's command with `--prepare`
  appended" and "when that process exits", not "asks the step" and "the
  step answers". A higher-level word is justified only when all of
  these hold:
  1. **A reader understands it without context.** "Asks the step"
     fails: nothing in it says a `--prepare` invocation is meant. "The
     step's `--prepare` run" passes, though it does not spell out the
     exact operation.
  2. **It saves more than it costs.** It earns its place when it stands
     for a series of connected operations that cannot be described more
     briefly: "a sync" for downloading, rendering and indexing a source.
     As a stand-in for one operation, like "pokes" for one signal, it
     almost never does: name the operation, or abbreviate it
     transparently.
  3. **A definition, if it needs one, is counted as a cost.** A word
     may be worth defining, but the definition is one more thing the
     reader has to hold. Weigh that against what the word saves; where
     the plain description is about as short, use it instead.
- **Match the level the reader asked for.** When someone asks how a
  thing works, keep higher-level words to what is strictly necessary,
  even when the text gets longer.
- **No invented metaphors and no coined terms.** Use an established
  term unmodified, or describe the behavior directly: "the fallback is
  never reached", not "the road not taken"; never decorate a known term
  into an unknown one ("a standing dirty bit").

**Structure: let the reader see what you refer to.**

- **Don't refer back to something the reader can't see.** State a list,
  number it, then refer to its items by number.
- **If it's a list, make it a list.** Three or more parallel items go in
  bullets or a numbered list, not a run of clauses in a paragraph.
- **Don't signpost structure that isn't there.** "The second half of X"
  needs a first half the reader can point at.

## Explain a change by what it does, before and after

**A PR description, a review, or any explanation of a change says what
was true before, what is true now, and what a person using Datalib
will notice.** Write it for a reader who knows the product but has not
read the code:

- **Start with the problem as something that could happen**: "an
  `<img>` in an email made the app re-measure the whole data root",
  not "closes the GET-walk hole".
- **Prefer the plain description to a word the code uses in a narrow
  sense**, and define the word the first time it appears when you keep
  it (§"Write plainspoken"). "Walks the data root" means nothing to a
  reader; "measures the size of every directory under the data root,
  which takes minutes on a large one" does.
- **Give each change a before, an after, and what a person notices.**
  "Nothing visible" is a valid answer; say it, so the reader knows it
  was checked. When a route or a flag changes, say what happens to a
  script or an agent still using the old one.

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
    dag/           `datalib-pipeline`: the DAG runner. It knows no program
                   by name, `datalib-step` included.
                   `//datalib/backend:bin` stages every shipped binary
                   under its public `datalib-*` name in one directory
                   (`:dist`); build that whenever you need to actually
                   run a pipeline.
    datalib_step/  `datalib-step`: the step program Datalib ships. A
                   step runs it with `command = "datalib-step run
                   <function> <type>"`, and it reads its params from
                   `DATALIB_DAG_PARAMS`.
    etl/           shared ingest machinery (raw stores, blob CAS, render
                   cursors) — the download side, and where a
                   downloader's dependencies stop.
    etl/files/     `datalib_etl_files`: what changed on disk for a
                   source that reads local files (file_changes_scan, the
                   fingerprint cache, the per-feed file checkpoint).
                   Only those sources link it.
    etl/web/       `datalib_etl_web`: what a source that reaches a web
                   service shares — `latchkey curl` with retries and
                   stops, HTTP playback, DAV, the owed-record
                   bookkeeping. Only those sources link it.
    etl/render/    `datalib_etl_render`: the render store, the
                   document-index load, `RenderCtx`. Everything that knows
                   `datalib_schema` sits here or above.
    etl/timeseries_render/ what the time-series render crates share.
    etl/time_series/ `datalib_etl_time_series`: the time-series stores
                   and their writers — a source's `render_time_series`
                   and the `time_series_index` — and the applet's reads.
    etl/providers/ <p>/ (ingest) + <p>_render/ (render) + <p>_config/
                   (config schema) per provider. Fifteen of the
                   file-backed ones scan a local tree through
                   etl/files/src/file_changes_scan.rs (claude_code and codex by way
                   of etl/agent_sessions/; file_inventory has its own walker
                   over etl/files/src/fswalk.rs); five mirror a SQLite
                   file through etl/sqlite_mirror/; three render time
                   series (airvisual, yolink, garmin); lightroom renders
                   a row per photo and writes no markdown. file_inventory,
                   media, apple_photos and linear have no <p>_render.
    etl/sqlite_mirror/ the table-for-table SQLite→doltlite mirror engine.
    table/         `BulkUpsertable`, alone.
    sqlite_open/   the options that open one file by its literal path;
                   every open of a store starts here.
    packages/      `datalib_packages`: a package's manifest, the lookup
                   and the pinned fetch that unpacks it under
                   `system/packages/` (docs/dev/packages.md).
    probe/         what "Check connection" and a picker's "Load" ask
                   and answer, alone.
    migrate_config/ `datalib-migrate-config`: rewrites a config in any
                   retired shape into the current one.
    lightroom_retime/ `datalib-lightroom-retime`: applies a file of
                   capture-time edits to a Lightroom catalog, the one
                   thing here that writes a catalog. Not shipped.
    runtime/       the data-root layout, which build this is
                   (`build_id`), the bundled-Node resolver (the `npx`
                   fallback is opt-in and loud), where the CUDA module
                   is and the embedding model pins. No first-party
                   dependencies, so anything can link it.
    embedding_models/ puts the pinned embedding model in place,
                   sha256-verified. Linked by the step and the applet.
    search_index/  the search index: keyword rows per message in
                   `search_*` tables of the grid index's file, and the
                   inference cache: each document's chunks keyed by their
                   prompt's hash, and the vectors, holding no text
                   (search_index/README.md).
    embedder/      text to vectors in process: llama.cpp
                   (third-party/llama_cpp) behind a small C shim, on
                   Metal on a mac, CUDA as a module on gnu Linux. Tested
                   against a golden of qmd's answers; README.md has the
                   facts, each a test in `embedding_facts`.
    store_meta/    `_datalib_meta`, the table every store carries naming
                   the build that wrote it and the shape it is in.
    sql/           `Sql`, `sql!` and `sql_id!`: every statement Datalib
                   runs, named where it is written; its one dep is sqlx.
    sql_work/      counts what every SQLite statement in a process does,
                   by the name it carries: runs, rows answered and walked,
                   the index each loop read, time; logs the ones that
                   walked many rows for few. Every binary that opens a
                   store installs it.
    sql_guard/     reads SQL a caller wrote, and nothing else: a
                   read-only open over SQLite's C API, the authorizer,
                   deadline and one-statement check, parameters by name
                   or position; behind the search applet's `POST /sql`.
    blob_store/    the blob store's shape and its reader: the one table
                   of a step's data `datalib-http` serves.
    core/          the app stores plus re-exports of `runtime`.
    query/         the search-bar grammar every grid shares; no
                   first-party deps.
    h3/            `datalib_h3`: where a thing is, as H3 cells over `h3o`;
                   a location is its finest cell, a cell a range of them.
                   README.md has the facts, each a test in `h3_facts`.
    html_entities/ reads `&amp;`-style character references back to text,
                   over quick-xml's HTML5 table; the one decoder.
    handle/        `Handle`: one identifier for a person (`email:`, `tel:`,
                   `slack:`), normalized; what renders write as
                   `data-handle`. Its one first-party dep is uuid_text.
    uuid_text/     a uuid read however it is written (dashed or not,
                   either case, at the end of a slug or a link) and
                   written lowercase with dashes; the one reader. No deps.
    store_dump/    a store's tables as the text of a golden, one row a
                   line; only tests link it.
    curated_overlay/ a checked-in `datalib_curated/` laid over a root and
                   checked to fit; the TNG test and the live bake share
                   it. Only tests link it.
    contact_schema/ `NormalizedContact`: a person as one source describes
                   them. Only the shape; contact-common renders it.
    photo_schema/  `NormalizedPhoto`: a photo or video as one source
                   describes it. Only the shape; the photos applet
                   reads nothing else.
    time_series_schema/ a time series, its tables, `TimeKind`,
                   `ValueType` and `Unit` (UCUM codes).
                   Only the shape.
    contacts/      the contacts app's store under `datalib_curated/`;
                   the `datalib_contacts` applet is its one writer.
    problems/      `datalib_problems`: the `problems` table every store
                   carries, one row per thing a step could not fully do
                   to one record, and its vocabulary.
    step_problems/ reads a step's problems where the step keeps them,
                   and the `problems.sqlite` a step with no store of its
                   own keeps them in.
    problem_mutes/ the problem rows a person muted, under
                   `datalib_curated/`; the
                   `unified_grid_and_search_index` applet is its one
                   writer, `datalib-http` reads it.
    unified_grid_and_search_index/ the grid index, the search index's
                   query side, the query language over them. Linked by
                   datalib-step and datalib-applet — never by
                   datalib-http or datalib-pipeline.
    applets/       `datalib-applet`: the applet host, and `datalib-applet
                   search`, the search box with no server.
    history/       a doltlite store's commit log; its one first-party dep
                   is `pin`, so datalib-http can serve it without
                   linking `etl`.
    http/          `datalib-http`: API server + sync loop + UI host +
                   applet gateway. Every route is behind a per-process
                   API token (src/auth.rs): read
                   `<root>/system/api-token`, send
                   `Authorization: Bearer <token>`.
    schema/        `grid_rows`/`edges`/`markdowns` row structs;
    app_schema/    disk usage, remote media, runs; both derive
                   DDL via `#[derive(PortableTable)]`.
  ui/          Vue frontend; every grid is SlickGrid, kept behind a few
               files so it can be swapped (docs/dev/cards.md § The grid).
  tauri/       the desktop shell (out of Bazel).
  reader/      `imbue-datalib-reader` on PyPI: a module that reads a store by
               calling libdoltlite, and its page. Its wheels are built by
               scripts/release/ (docs/dev/release_steps.md).
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

`datalib-pipeline sync <config.toml>` runs a DAG of subprocess steps. A `[[groups]]`
entry is one thing on the Manage screen (a source is a group with a
`type`); a `[[steps]]` entry is an `id` — the one tree it writes, whose
first part is its group — and the `command` it runs; `inputs` name steps
by that id, or every step of one function by a pattern
(`*/render_markdown`), and are the edges; an `[[applets]]` entry is a
server the gateway spawns. Datalib's own steps run `datalib-step run
<function> [<type>]` (so their ids are `<group>/<function>`) and write
out the `reads` and `locks` they need; the `[[programs]]` entry for
`datalib-step` lists the verbs it takes (`--reset`, `--prepare`,
`--compact`, `--collect`), and the runner sends no verb a program does
not list. What each is made of:
[`config_model.md`](docs/dev/config_model.md) §"Three kinds of entry".

Each source has an `ingest` step and (most) a `render_markdown` step, and
`unified_grid_and_search_index` under `unified_grid_and_search_index` reads every render tree its `inputs`
name (`*/render_markdown`, every source's) into the SQL index the grid
reads. It then puts each source in the keyword search, the `search_*`
tables in the same file, but those its `params.not_searched` lists. A
source searched by meaning has one more step of its own, `embed`, which
reads `unified_grid_and_search_index`, cuts the source's documents into
chunks and fills their vectors in
`unified_grid_and_search_index/search_index/inference_cache.sqlite`, so
the slow embedding can be turned off or run by hand per source; nothing
else loads the embedding model. `unified_grid_and_search_index` also
sweeps the vectors no chunk names and reports what search holds of each
source. `embedding_map` reads every `embed` and lays the embeddings out
on a plane for the map card
(`datalib/backend/embedding_map/README.md`).
A source whose data is also time series (Garmin, YoLink, AirVisual) has a
`render_time_series` step beside its `render_markdown`, and
`time_series_index` under `unified_time_series_index` indexes every
such store without copying its points (`docs/dev/time_series.md`). The document index is
queried by the `unified_grid_and_search_index` applet; `datalib-http`
reads the stores' commit logs (`history/`) and never their rows, except
a step's blob store, whose one table it serves at `/data/…`
(`datalib/backend/blob_store/README.md`), and every step's `problems`,
each read where its step keeps it, which it counts, less the muted
ones, for the Manage counts (`datalib/backend/problem_mutes/README.md`). A render
store is readable at every commit: the documents between two checkpoints
share one transaction. The loop's record — each step's state now, its
last run and success, each sink's version — is in
`system/supervisor.sqlite`, and a Manage row's Status is that state. A
config entry the loader cannot use costs that entry and nothing else;
`datalib-pipeline check <config>` says what went and why. A config the app
cannot serve anything from comes back as `app_ready: false` and the UI
shows `ConfigErrorView`, live in both directions. The http server keeps
`datalib-pipeline serve` running as a child for its life
(`http/src/supervisor.rs`), which holds `runner-lock` and runs the loop
(`dag/src/supervisor/serve.rs`), and hears what no store holds from the
child's stderr; a sync, a stop, a step turned off is a row the server writes
there (`POST /api/requests`, `/api/steps/<id>/turn_off`); the Manage tab
edits the config; a root with no config gets the launcher and the
first-run screen, which writes Datalib's starter config
(`http/src/starter.rs`: the grid and search index and the applets).
`datalib-pipeline init` writes a config with no steps instead.

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

84 at the last count. If that number climbs, something took a dependency
it should not have.

## The grid_rows union table

The grid is one denormalized table, `grid_rows` (the `GridRow` struct in
`datalib/backend/schema/src/grid_rows.rs`), which `unified_grid_and_search_index` fills
from every source's render store and the `unified_grid_and_search_index` applet reads
with one SELECT — no per-provider branches in the query path.
`docs/dev/grid_rows.md` has the checklist for adding a column.

## Rendered markdown is never parsed back

The render step hands each document over as markdown, section by
section, and its render store keeps it; there are no `.md` files. The
backend serves a document **verbatim**, as the store spells it, and
never parses it back. Structured fields come from
`grid_rows`. Per-section anchors come from the
`<div id="m-{uuid}" data-section-uuid="{uuid}" class="msg …">` wrappers
the renderer emits — those two attributes are load-bearing
(`ui/src/cards/chatSections.js::messageAncestor`). Where each message
begins comes from the projection too: a renderer hands its document over
as `RenderedMarkdown::sections`, and the render store keeps their
layout
([`data_architecture_parse_and_render.md`](docs/dev/data_architecture_parse_and_render.md)
§"One section cannot reach the next"). If you find yourself writing a
markdown parser in the backend, stop — add the field to `grid_rows`
instead.

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
Every fact in `doltlite.md` is a test, named at its top, so a doltlite
bump that moves one fails by name.

The rules, none optional. How our code enforces them is in
`datalib/backend/etl/README.md` §"Connection pools":

- **One writer per file.** Doltlite keeps uncommitted rows per branch
  *in the file*, so two writers on one branch commit each other's
  half-written rows, and doltlite itself only makes a second writer
  wait its turn, never refuses it
  ([locks and writers](docs/dev/doltlite.md#locks-and-writers)). Our
  per-file lock refuses it; `app_stores.md` names each store's one
  writer. (The grid index is plain SQLite, not doltlite, and needs none
  of these rules.) A download takes its store as an input
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
  read-only open of `<file>@<hash>`. Render and `unified_grid_and_search_index` read their
  input stores the last way (`doltlite_raw::open_reader`), so their
  queries name the tables themselves. A process that *moves a ref*,
  even on its own branch, is a writer.
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
only used for fixture / test tooling and scripts; nothing that ships is
Python but the one standard-library module in `datalib/reader/`.

A version bump also changes the wheel hashes `MODULE.bazel.lock` keeps
under `facts`, and a local build can keep the old ones: Bazel's hidden
copy of the lockfile, `$(bazel info output_base)/MODULE.bazel.lock`,
holds the pip extension's last run. CI then refuses the lock ("has
changed its facts"). Delete that file, then `bazelisk build --nobuild //...`.

## Running tests

**"Build green" means `bazelisk test //...` passes — nothing less.** A
narrower invocation is fine for the inner loop, but don't call the tree
green based on one; say what you actually ran. Before pushing, run the
whole local gate (what it runs, and with which flags:
[`testing.md`](docs/dev/testing.md#source-of-truth-bazelisk-test-)):

```bash
bazelisk run //:precommit          # the one command to run before pushing
```

Measured on one warm mac:

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

**Bazel is the only supported driver**: `cargo test` / `pnpm test`
bypass its cache and sandbox and can disagree with CI, and anything
under `bazel-bin/tests/fixtures/ingested/` is a genrule output only
bazel rebuilds. Insta snapshots, coverage and slow runs:
[`testing.md`](docs/dev/testing.md), [`coverage.md`](docs/dev/coverage.md),
[`ci.md`](docs/dev/ci.md).

**A green suite cannot show a sync that is right but slow**: work
redone, fetches made one at a time, an ETA that never moves, files
left behind. After changing a download, a render, an index or a stored
layout, sync a real root and read its run store
([`testing.md`](docs/dev/testing.md) §"Auditing a real sync's logs").

## Tests wait on the observable, never on the clock

**A test never `sleep`s to order two writers, or to give something
time to happen; it waits for the thing it is about to assert.** A
sleep that is long enough on a warm mac is short on a loaded CI runner,
and a sleep that is long enough on CI makes every local run slower than
it needs to be. Poll the row, the
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
  `embedding_models`); where the variable itself is what's under test, take
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
./run-app.sh [<data_root>]                            # build and launch the desktop app (macOS)
bazel-bin/datalib/backend/bin/datalib-pipeline sync <data_root>
```

## "Datalib" in prose, `datalib` in code

**The product is Datalib, capitalized, wherever a doc's prose names
it**, headings included. Lowercase `datalib` is an
identifier — a crate, a binary (`datalib-step`), a path, the repo, the
`datalib:` link scheme — and goes in backticks when prose mentions it.

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
is `source_id` everywhere. The search filter is `source:` (`source_id:`
still works) and the column a person sees is "Source".

## A local file or folder: ask `file_changes_scan` what changed

**Don't walk a folder or re-read an input yourself to learn whether it
changed; ask `datalib_etl_files::file_changes_scan`.** An unchanged
input costs a `stat`, and a changed one is read once, its hash with its
bytes; `file_checkpoint` keeps this source's `path → blake3` cursor to
diff against. The recipe is `datalib/backend/etl/files/README.md` §"Answering "did it change?"
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
| what a run is for | `RunKind` | `app_schema/src/runs/run.rs` |
| why a step failed | `FailureKind` | `dag/src/step.rs` |
| what the run store names | `LiveState` | `runs/src/lib.rs` |
| a log line's severity and pipe | `LogLevel`, `Stream` | `app_schema/src/runs/log.rs` |
| what the loop made of a step (`steps.state`) | `StateKind` | `dag/src/supervisor/tick.rs` |
| what a waiting step waits for (`steps.waiting_for`) | `WaitKind` | `dag/src/supervisor/tick.rs` |
| how a sync request ended | `RequestOutcome` | `dag/src/supervisor/store.rs` |
| where a step stands for one sync request | `RequestStepPhase` | `http/src/request_progress.rs` |
| a browser-login attempt, and what it is doing | `ConnectState`, `ConnectPhase` | `http/src/connect.rs` |
| who names a latchkey account a browser login adds | `AccountNaming` | `http/src/connect.rs` |
| what kind of trouble a sign-in or probe ran into | `IssueKind` | `probe/src/issue.rs` (`datalib_probe`) |
| a probe the wizard polls | `ProbeState` | `http/src/probe.rs` |
| how a pass (the prepare pass at a start, a wipe that frees disk) stands with one step | `PassState` | `dag/src/supervisor/pass.rs` |
| the list a picker loads | `ProbeList` | `probe/src/lib.rs` (`datalib_probe`) |
| the `grid_rows.provider` tag | `Provider` | `schema/src/providers.rs` |
| what a step could not fully do to a record | `Outcome`, `Reason`, `ScopeKind`, `Severity`, `Stage` | `problems/src/lib.rs` (`datalib_problems`) |
| a configured entry upstream does not have; a listing or phase a run could not do | `ProblemReason`, `RunProblemKind` | `etl/src/download_problems.rs` |
| how a diff group's row differs between two renders | `DiffStatus` | `schema/src/diff_status.rs` |
| what an edge means, stored as its number | `EdgeKind` | `schema/src/edges.rs` (`datalib_schema`) |
| what a term is to its grid row | `SearchTermKind` | `schema/src/search_terms.rs` (`datalib_schema`) |
| a config's source type | `SourceType` | `datalib_step/src/source_type.rs` |
| whether an ingest method reaches a service or reads files | `Reach` | `source_common/src/lib.rs` |
| which of Datalib's stores a file is, in its `_datalib_meta` | `StoreKind` | `store_meta/src/lib.rs` (`datalib_store_meta`) |
| what namespace a handle is in | `HandleKind` | `handle/src/lib.rs` (`datalib_handle`) |
| what a time-series point's `t` counts | `TimeKind` | `time_series_schema/src/lib.rs` (`datalib_time_series_schema`) |
| what every value of a time series is | `ValueType` | `time_series_schema/src/lib.rs` (`datalib_time_series_schema`) |
| a contact is a person or a group; how one is reached | `ContactKind`, `Medium` | `contact_schema/src/lib.rs` (`datalib_contact_schema`) |
| a photo or a video; where its time's offset came from | `MediaKind`, `OffsetFrom` | `photo_schema/src/lib.rs` (`datalib_photo_schema`) |
| which picture of a photo the applet serves | `Size` | `applets/src/photos/pictures.rs` |
| what a call to a contacts-app route means | `RouteKind` | `applets/src/datalib_contacts/api.rs` |
| who made a link or a label: a person, or an agent waiting for one | `DecidedHow` | `contacts/src/lib.rs` (`datalib_contacts`) |
| what linking a handle another contact has does: refuse, share it, or move it | `WhenHeld` | `contacts/src/lib.rs` (`datalib_contacts`) |
| who muted a problem row: a person, or an agent | `MutedHow` | `problem_mutes/src/lib.rs` (`datalib_problem_mutes`) |
| what a contact's field says | `FieldKind` | `contacts/src/lib.rs` (`datalib_contacts`) |

The TypeScript side gets the ones the UI switches on as string-literal
unions in `datalib/ui/src/generated/enums.ts`, written from the Rust by
`bazel run //datalib/backend/http:ui_enums.update`; a test in
`datalib_http` fails when it drifts. Add an enum to that generator
rather than hand-writing a union in `api.ts`. The environment variables
one process sets for another live once, in `datalib/backend/env`.

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

## No fallbacks, no workarounds: one way to do it

**No fallbacks, no workarounds. Wherever possible, just one way to do
it.** A second path is a path the tests rarely take, and the dangerous
ones *succeed*: a correct answer reached the slow or lossy way raises no
error, and a read that should have failed returns less. When the input
is not what the one way expects, fail loudly, naming what was found and
what was wanted; do not branch around it.

- **A store is in this build's shape or it is refused.** Nothing reads
  around a table, a column or an index an older build left out: the
  prepare pass brings every store to this build's shape before anything
  reads it, and every open checks the shape `_datalib_meta` records
  (`datalib/backend/store_meta/README.md`). A "no such table" or "no
  such column" from a store whose shape was checked is a bug, not "an
  older store".
- **A statement names the index it relies on** (`INDEXED BY`): an index
  is part of the store's shape, so it is there.
- **One reader, one writer, one spelling.** Where two functions do one
  job, delete one; where a value has two names, keep one and migrate the
  other.

The exception is input a **person** wrote, which keeps a compatibility
path (§"Keep a forward path for existing data"), and upstream data,
whose shape is not ours to choose.

**An error or a warning about a record goes through `problems`, never
only to the log.** A record a download could not fetch goes through
`record_object_attempt` / `record_object_error`; everything else a
download could not do goes into the one `RunProblems` its `fetch` is
handed (`run_problems::collecting`;
[`data_architecture_ingestion.md`](docs/dev/data_architecture_ingestion.md)
§"Error handling"). A record render could not fully project goes on its
document's `RenderedMarkdown::problems`, or through
`RenderCtx::report_*` when there is no document yet. Each step keeps
its rows in its own stores, and a person sees them there, as the Manage
row's count and the banner above the document
([`etl/README.md`](datalib/backend/etl/README.md) §"Problems stay in
the step that found them"). A `warn!` alone reaches nobody.

## Every statement is a named `Sql`

**A statement is named where it is written, for what it does:**
`sql!("runs.count_tables", "SELECT …").query()`, or for text built while
running, `Sql::dynamic(sql_id!("etl.render.grid.settle_nodes"), text)`
(`datalib/backend/sql`). The name is `<package>.<what_it_does>`, verb
first, and it is what a statement's runs, rows scanned, index use and time
are counted under (`docs/dev/logging.md` §"Where a line comes from", "What
every statement did is counted under its name").
Clippy refuses sqlx's own entry points (`sqlx::query`, `raw_sql`, an
executor handed a `&str`), so a statement without a name does not build,
and `//:lint_repo` refuses a name written twice. A test writes
`test_sql!("…")` / `test_sql_id!()`, named for its module and line.

**Schema text stays plain text and is named where it runs**
(`Sql::ddl(sql_id!(…), DDL)`, filed under the table it makes): a store's
recorded shape is a hash over its DDL, and a name in that text would
move every store's shape.

## Dynamic SQL needs a reason

sqlx 0.9 only accepts `&'static str` as a query string, and
`Sql::dynamic` is the one way past that: an assertion *you* make, with a
comment saying why the text is safe. Two patterns cover almost
everything: a `?` run sized from `chunk.len()` with every value bound, and
table/column names that are `&'static str` at every callsite. Anything
from upstream data is quoted (`doltlite_raw::quote_ident`) or
bound. **A set of values to match is one bound JSON array**,
`IN (SELECT value FROM json_each(?))`, never a `?` per value: SQLite
binds at most 32,766 values per statement, and only a real user's data
gets there (`datalib/backend/etl/README.md` §"Binding a set of values").

## Card source is data

**Nothing that came from outside becomes code.** A card's source, in a
URL, a saved layout or a card's header, is one call of a known view or
component with JSON5 values. It is parsed, never evaluated, and the app
page's CSP has no `'unsafe-eval'` (`docs/dev/cards.md` § "Card
source"). Any site can send the browser to a card URL, so **opening a
card never writes**: a card's arguments come from whoever wrote the
link. `//:lint_repo` refuses an evaluator anywhere in `datalib/ui/src`,
and HTML drawn from a string outside its short allowlist.

## A safety check names what it allows, not what it refuses

**Treat everything as dangerous, then let through a short list of
things known to be safe.** A list of dangerous things is always missing
one. The server once sandboxed only the content types it knew could run
script, and browsers render `text/xsl` and a comma list such as
`text/plain,text/html` as pages too, so a sender's attachment ran
script in the app (#1539). So serve every file sandboxed and keep only
the types on a safe list; accept a path only in its canonical form
under its root; pass a child process only the environment it needs. A
deny-list is fine as a second layer behind such a check (a lint, a
sanitizer behind a CSP), never as the only one. Don't add one; when
you find one standing alone, turn it around.

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

**`Z` means the clock is unknown; `+00:00` means it was UTC** (RFC
9557). `-00:00`, RFC 3339's old spelling of "unknown", is read and never
written. A rendered time whose source names no clock (a Unix stamp, a
service's own `Z`) is written `Z` (`datalib_time::with_unknown_offset`);
a stamp we mint, whose clock we know, never ends in `Z`. Why, and which
clock each source's messages are on:
[`rendered_documents.md`](docs/user/rendered_documents.md#a-time).

## Auth (web API)

Every web source that needs a credential signs in through latchkey,
and its requests go out as `latchkey curl`:
[`docs/dev/latchkey.md`](docs/dev/latchkey.md). A URL that carries its
own authority skips latchkey and goes out as plain `curl` through the
same HTTP layer (`HttpRequest::plain`): YoLink's signed CSV downloads,
Notion's pre-signed file links. Such a URL came from upstream, so plain
curl reads no `~/.curlrc`, speaks https only, takes the URL after
`--url` and stops at the request's `max_bytes`; that is
`HttpError::TooLarge`, which a source records as a size-limit warning,
not a failure.
Cloudflare-fronted hosts go through the bundled `curl-impersonate`
([`docs/dev/curl_impersonate.md`](docs/dev/curl_impersonate.md)); if
Cloudflare still 403s, the IP or user agent may be flagged — wait it
out or swap networks.
