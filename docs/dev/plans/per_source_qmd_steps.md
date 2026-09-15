# Plan: per-source `qmd_index` and `qmd_embed` steps

**Status: proposal (2026-09-15), nothing built.** Per
[`AGENTS.md`](../../../AGENTS.md), this file describes work we intend to
do, not the tree. The claims about *today's* behavior carry a
`file:line` or a measurement, and every measurement below was taken on
2026-09-15 against qmd 2.8.3 (the pin in `runtime/src/qmd.rs`) on a mac;
the recipe is at the end so it can be re-run.

## The ask

Today the qmd index is one global step, `unified_index/qmd_index`, that
fans in every source's `render_markdown` tree and runs `qmd update` then
`qmd embed` over all of it (`datalib_step/src/qmd_index.rs`,
`qmd_indexer/src/lib.rs::run_index`). Since per-source collections
landed, the *index* already has a per-source shape — one qmd collection
per group — but the *work* is still one step: a giant source's embedding
holds every other source's search freshness hostage, and there is no way
to run, stop or resume one source's indexing on its own.

The change: each source gets its own chain, `ingest → render_markdown →
qmd_index → qmd_embed`, so the Manage screen shows four rows per group,
and each source's keyword indexing and its (much slower) embedding are
separately runnable, stoppable and resumable. `grid_index` stays a
single fan-in; nothing about it changes. The qmd store stays one SQLite
file, and one `qmd mcp` daemon keeps serving every collection from it.

## What the experiments established

All against the TNG fixture's rendered tree (16 groups, 79 documents),
with one collection per group, exactly as the shipped step registers
them.

1. **`qmd update` cannot be scoped to a collection.** `updateCollections`
   (`third-party/qmd/src/cli/qmd.ts:895`) loops over
   `listCollections(db)` — every collection in the store — and there is
   no `-c` on `update` in 2.8.3 or on upstream `main` (checked
   2026-09-15). Only `embed` takes `-c`, and only one name
   (`qmd.ts:4646`). So a per-source step that shells `qmd update` would
   re-hash every source's files on every run: O(sources × corpus), and a
   step whose declared input is one tree while it reads all of them.

2. **Writing a source's `documents`/`content` rows ourselves works, and
   qmd cannot tell the difference.** In 2.8.3 `reindexCollection`
   (`store.ts:1605`) does exactly this per file: `path` is the literal
   relative path (the `handelize` mangling `qmd_index_ui.md` describes is
   gone — it is display-only now, `store.ts:1648`), `hash` is a plain
   SHA-256 of the UTF-8 bytes (`store.ts:2860`), `title` is the first
   `#`/`##` heading (`store.ts:2866`), FTS rows come from triggers on
   `documents` (schema, `documents_ai`/`_au`/`_ad`). Measured: after an
   `UPDATE documents SET hash=…` plus an `INSERT OR IGNORE INTO content`
   from the sqlite3 shell, `qmd search` found the new text, `qmd embed -c`
   embedded it, `qmd vsearch` found it, and a following `qmd update`
   reported it `unchanged` — our row is byte-for-byte what qmd would have
   written. (One quirk: the `INSERT … ON CONFLICT DO UPDATE` upsert
   qmd's own `insertDocument` uses failed with `constraint failed (19)`
   from the stock sqlite3 3.51 shell, while a plain UPDATE and a plain
   INSERT both worked. Use the two-statement form from Rust and add a
   test that pins it.)

3. **`qmd embed -c <group>` is scoped, and it is the only lever needed
   on the embed side.** Embedding `slack` alone left every other
   collection at zero vectors (per-collection counts by SQL). The step
   reads pending per collection with qmd's own query
   (`getHashesNeedingEmbedding`, `store.ts:2520`): active documents
   whose hash has no `content_vectors` rows for the current
   `(model, embed_fingerprint)`, or fewer than `total_chunks`.

4. **`update` and `embed` can overlap on one file.** qmd 2.8.3 opens the
   store with `busy_timeout = 120000` and WAL (`src/db.ts:100`),
   explicitly "so parallel processes (e.g. an `update` or `query` racing
   a long `embed`) queue at batch boundaries instead of failing".
   Measured: three `qmd update` runs during one full `qmd embed`, all
   four exited 0, no errors, the vector count was right afterwards.

5. **Two `embed`s cannot overlap, and the loser exits 0.** 2.8.3 has a
   process lock (`src/cli/embed-lock.ts`, `.qmd-embed.lock` beside the
   index) because concurrent embeds race on `vectors_vec`. Measured: the
   second of two concurrent `embed -c` calls printed `Another embed
   process is already running. Skipping.` and **exited 0 having embedded
   nothing**. A step that trusts that exit code reports success for work
   it did not do — the fallback-that-succeeds `AGENTS.md` warns about.
   The lock file is recovered by PID check after a crash (measured: a
   SIGTERM'd embed left the file; the next embed took it and finished).

6. **`qmd embed` stops itself after 30 minutes and still exits 0.**
   `DEFAULT_EMBED_MAX_DURATION_MS = 30 * 60 * 1000` (`store.ts:97`); on
   the cap it prints `⚠ Session expired — skipping remaining document
   batches` (`store.ts:2073`) and then `✓ Done!` (`qmd.ts:2244`).
   `--timeout <minutes>` sets it, `0` disables it. **This is a live bug
   in today's global step**: a root whose first embed needs more than
   half an hour reports `Succeeded` with most documents unembedded, and
   nothing re-runs it until a render moves. Worth an issue on its own;
   the design below turns the cap into a feature.

7. **Embedding resumes.** Vectors are committed per batch; an interrupted
   run leaves what it finished, and the pending query above counts the
   rest (partial documents count as pending, and
   `removeIncompleteEmbeddings` cleans them up). A re-run picks up where
   it stopped.

8. **Cost, for scale.** 79 documents: `update` 0.1s, full embed 9–11s on
   this mac's GPU (CI is CPU-only and the fixture's embed action is
   ~90s). Model load is ~2–3s per `qmd embed` invocation.

## The design

### Two functions per source, one shared store

```
<g>/ingest  →  <g>/render_markdown  →  <g>/qmd_index  →  <g>/qmd_embed
                       │
                       └────────────→  unified_index/grid_index   (unchanged)
```

- **`qmd_index`** (built-in function, `datalib-step`): reconcile this
  group's collection in the shared store from its `render_markdown`
  tree. Register the collection if missing (`qmd collection add`,
  idempotent, the one qmd shell-out this step keeps — it also creates
  the store and its schema on a fresh root). Then, in Rust over sqlx:
  walk `<g>/render_markdown/**/*.md`, SHA-256 each file, compare
  against the collection's `documents` rows, and insert/update/
  deactivate exactly as `reindexCollection` does (finding 2). One
  transaction per batch under `busy_timeout`, so a reader sees whole
  documents. Reports a version: a hash over the collection's sorted
  active `(path, hash)` pairs plus the qmd pin (next section), which is
  content-derived and stable, so `qmd_embed` re-runs only when the
  document set or the qmd version actually moved. Fast: seconds per
  source, no model.
- **`qmd_embed`** (built-in function): `qmd embed -c <g>` in a loop
  until this collection's pending count is zero or the run's budget is
  spent (below). Optional per source — a source without this step is
  keyword-searchable and not semantically searchable, which is what the
  `Embedded` column already knows how to show.

`grid_index` keeps its fan-in over every `render_markdown`. The
`unified_index` group loses its `qmd_index` step and keeps `grid_index`
plus the applet.

**The shared file.** Both steps write `index.sqlite`, which is not the
tree either step's id names. The id-is-the-tree rule
(`dag/README.md` §"A step is (group, function)") stays true in the
useful sense: each step owns `<g>/qmd_index/` and `<g>/qmd_embed/` and
writes a small `state.json` there (collection name, document count,
version; pending/embedded counts, model, fingerprint, last session) —
what the Manage screen's group row can read off the folder without
opening qmd's file. The store itself stays where it is,
`unified_index/qmd_index/qmd/index.sqlite` (`QMD_INDEX_REL`). That
directory is spelled like a step tree and after this change no step is
named that; moving it to `unified_index/qmd/` is cosmetic, costs a path
change in half a dozen files plus the fixture tar layout, and is left
for a follow-up (decided 2026-09-15). `unified_index/` keeps its
`CACHEDIR.TAG`.

### What makes a qmd bump re-embed: the `render_version` pattern

A `LAYOUT_VERSION` bump re-renders every chat source without the runner
knowing: the render step stores its `render_params` beside its cursor
and re-renders every bucket when they differ (`render/src/processor.rs:39`,
`datalib_step/src/render.rs:334`). It gets the chance to check because
it runs every sync — ingest inserts a `sync_runs` row per run, so its
dolt HEAD moves every run and render is stale every run; render then
rewrites its cursor with the new raw commit, so *its* HEAD moves every
run too. The pipeline runs top to bottom on every sync, and each step's
incrementality is its own.

`qmd_index` inherits that: its input moves every sync, so it runs every
sync (and re-hashing one source's tree is the cost — see the
follow-ups). It folds the qmd pin into the version it reports,
`<hash of the collection's (path, hash) set>:<DEFAULT_QMD_VERSION>`,
which is honest — the rows it writes are that qmd's format. A qmd bump
moves the version, `qmd_embed` is stale, runs, finds pending > 0 and
embeds. (qmd's embedding fingerprint, `store.ts:114`, is the model name
plus chunking and formatting constants, all of which change only with
the qmd version, so the pin is an exact proxy.) A re-render is the same
story one level up: the bytes change, the hashes change, the documents
are pending.

`qmd pull` — today run on every index pass (`qmd_indexer/src/lib.rs`,
"Pull BEFORE embed"), a network round-trip per sync — moves into
`qmd_embed`, gated on `models_present`, under the embed lock so two
sources never pull at once. It fetches the query-expansion and reranker
models as well as the embedding one, which is what search needs, so it
stays a pull of all three.

### Three locks around embedding, each for a different failure

Findings 4 and 5: any number of `qmd_index` steps may run beside one
`qmd embed`, but two embeds must not overlap, and qmd's own refusal is
silent. Three layers, innermost first:

1. **The step never trusts qmd's exit code.** After every `qmd embed`
   invocation the step re-reads the pending count. Pending unchanged
   and the output contained `Another embed process` → the step failed
   to get the lock, and says so. This is what makes the other two
   layers *optimizations* rather than the only thing keeping the index
   honest.
2. **The step takes `flock` on `unified_index/qmd/.datalib-embed.lock`
   before spawning qmd.** Blocking, with `progress_message("waiting for
   another source's embedding to finish")`. Correct on its own; costs a
   parallelism slot while waiting.
3. **The scheduler does not dispatch two steps that share a lock key.**
   A new optional `[[steps]]` key, `lock = "qmd_embed"`: a ready step
   whose key is held by a running step waits in the ready set instead of
   taking a slot (`scheduler.rs` dispatch loop, a `HashSet<String>` of
   held keys — small). The runner interprets no function; the wizard and
   the config examples write the key on every `qmd_embed` step, and a
   custom step can use it too. Without it (a hand-written config that
   forgot), layer 2 keeps it correct and the run is merely slower —
   which matters for the case this whole plan is for: four sources
   embedding in turn must not pin three of four slots on `flock` while
   ingests queue behind them.

### Run to completion; budgets are opt-in

qmd's own embed session cap is 30 minutes (finding 6). The step passes
`--timeout 0`, so an embed runs until the collection is done or someone
presses Stop — the step loops `qmd embed -c <g> --timeout 0`, re-reading
pending between invocations, until pending is zero. Cancel (SIGTERM)
maps to `Cancelled` and the next run resumes where it stopped
(finding 7); the Stop button is the lever, not a timer (decided
2026-09-15).

What that costs, said plainly: one runner per root (`dag/README.md`
§"Two locks") and one job at a time in the http worker, so while a
giant source embeds, a "Sync now" on any other source waits in the
queue until the embed finishes or is stopped. Stop cancels the whole
run today; cancelling one step and letting the rest of the run continue
is a follow-up worth doing once this is in.

For anyone who wants the bite-sized behavior instead, `params.budget_minutes`
(default `0`, unbounded) caps one run's embedding. A budgeted run that
ends with work left must **not** record success — the scheduler re-runs
a step only when an input moved, and nothing moves while it embeds — so
it exits with a new failure kind, `Incomplete` ("stopped on purpose
with work left; resume next run"): zero retries, its own word on the
Manage screen rather than an error. Every later run resumes it for
another budget's worth.

### Progress and "work left"

Both steps report through the existing metric channel
(`docs/dev/plans/completed/logs_and_metrics.md` §"Progress becomes
metrics"): absolute gauges, `queued` for work left, rates derived by
the UI from samples.

- `qmd_index` owns its loop, so it is exact: `documents_total`,
  `documents_done`, `queued = total − done`, plus
  `indexed`/`updated`/`unchanged`/`removed` at the end.
- `qmd_embed` polls the store while qmd runs — the design
  `qmd_index_ui.md` §"Rule 2" already argued for: a thread on a
  read-only connection every ~1s runs the pending query for this
  collection and emits `queued = pending documents`,
  `embedded = active − pending`, and `chunks` from `content_vectors`.
  This measures what actually landed, not what qmd printed (qmd's
  progress is TTY-only, `qmd.ts:2231`), and it keeps working across qmd
  bumps because the schema is the stable interface. `progress_message`
  covers the phases the numbers can't: "loading model", "waiting for
  lock", "budget spent, N documents left".

The grid's `Indexed`/`Embedded` columns and the "N of M searchable"
line read the same store and need no change; the `qmd_state` endpoint
only moves with `QMD_INDEX_REL`.

### The e2e fixture

`//tests/fixtures:ingested_tng_qmd` embeds all 16 groups in one ~90s
CPU action, keyed on `qmd_md.tar`. After this change the standalone
`qmd_indexer` CLI (the same library `datalib-step` uses) takes
`--embed <group>,…`: index every group (fast — finding 8, and needed so
keyword search and the `Indexed` column cover the whole fixture), embed
only the named ones. Two groups (say `slack` and `claude-api`, the two
with the most cross-document structure) is enough for every
vector-search test in the tree, and cuts the action to a fraction of
the corpus. Tests that assert "every document embedded"
(`qmd_index_state_test`, the e2e `Embedded` column spec) pin the two
groups instead, and `qmd_daemon_scope.rs`'s scoped searches use them.

### Search while embedding

`QmdDaemon` respawns `qmd mcp` whenever `index.sqlite`'s mtime differs
from the one it spawned against (`daemon.rs:180`). The mtime moves on
every embed batch, so during a long embed **every search reloads the
model**. Before this ships, measure whether a live `qmd mcp` sees rows
committed after it started (WAL + fresh statements say yes; whether qmd
caches a document list in memory is the question). If it does, drop the
mtime respawn or restrict it to the store being *replaced*.

### Config, wizard, migration

A source's TOML grows two steps (the second optional):

```toml
[[steps]]
group = "slack"
function = "qmd_index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "slack"
function = "qmd_embed"
inputs = ["slack/qmd_index"]
lock = "qmd_embed"
```

The `unified_index` group keeps `grid_index` and the applet and loses
its `qmd_index` step.

- **Wizard** (`SourceWizard.vue`, `sourceSteps.ts`): writes both, with a
  "Semantic search" checkbox for the embed step, on by default. The regex that appends
  a new render step to the `unified_index/*` inputs
  (`sourceSteps.ts:786`) narrows to `grid_index`.
- **Manage screen**: `phaseOfFunction` gains `qmd_index` → "index" and
  `qmd_embed` → "embed"; group status aggregates as it does. The row
  menu's "Sync now" on a `qmd_embed` row is already "this step and its
  dependents" (`scheduler.rs::runnable_subgraph`), which for a leaf is
  just the embed — the resume button, for free.
- **`datalib-migrate-config`** gains the rewrite: drop
  `unified_index/qmd_index`, add the two steps under every group that
  fed it. The loader warns on the old shape and names the tool, as it
  does for the last migration.
- **Scaffold** (`http/src/lib.rs::scaffold_toml`), the three
  `docs/user/config_examples/`, `configs/dag_example.toml`,
  `agent_config_guide.md`, `first_time_user.md`: same shape.

### What lands where

| piece | crate |
|---|---|
| Rust writer for one collection (`index_group`), pending query, embed loop with budget and lock detection (`embed_group`) | `qmd_indexer` (already shared by the fixture and `datalib-step`; gains an sqlx dependency, which it does not have today) |
| `Function::QmdEmbed`, `qmd_index.rs` rewrite, new `qmd_embed.rs`, `state.json` | `datalib_step` |
| `lock` key, `FailureKind::Incomplete` + `RunState` word, dispatch gate | `dag` |
| `--embed` on the CLI, `--embed-groups` through `build_qmd_index.py` | `qmd_indexer/src/main.rs`, `tests/fixtures/` |
| wizard, phases, labels, inputs regex, `DagRunState` union | `ui` |
| migrator rewrite | `migrate_config` |
| docs above | `docs/`, `configs/` |

## Slices

1. **Library.** `qmd_indexer::index_group` (Rust writer) and
   `embed_group` (loop, budget, lock detection, pending query), with a
   test that writes rows, runs `qmd update`, and asserts `unchanged`
   (the byte-identity claim in finding 2) — plus the two-embeds test
   that pins qmd's silent skip so a version bump that changes the
   message is caught. `run_index` composes them; the fixture and the
   global step keep working unchanged. Ships alone.
2. **Runner.** `lock` key, `Incomplete`, the dispatch gate; unit tests
   in `scheduler.rs`.
3. **Steps.** `qmd_embed` function, `qmd_index` rewrite,
   `state.json`. Config examples and scaffold. At this
   point a hand-written config runs the new shape.
4. **UI and migration.** Wizard, Manage rows, migrator, docs.
5. **Fixture.** `--embed` and the two-group fixture; retarget the
   tests that assumed everything was embedded.
6. **Follow-ups**, deliberately out of scope: index incrementally from
   the render store's `dolt_diff` cursor instead of re-hashing the
   tree (the walk is O(source) per run, fine until a source has ~10⁵
   files); CJK normalization for FTS (`normalizeCjkForFTS`,
   `store.ts:875` — the trigger path skips it, qmd's own path applies
   it; a small port); the daemon respawn above; a "Re-embed" row action
   (`qmd embed -f -c`); cancelling one step without the run; moving the
   store to `unified_index/qmd/`.

## Decisions (2026-09-15)

1. Embedding is **on by default** for a new source in the wizard; the
   fixture is where it is off.
2. The store **stays** at `unified_index/qmd_index/`; the rename is a
   cosmetic follow-up.
3. **`Incomplete`** is a new failure kind, not a reuse of `Transient`.
4. **`lock`** is a visible `[[steps]]` key, written by the wizard and
   usable by custom steps.
5. The fixture embeds **`slack` and `claude-api`**.
6. A qmd bump re-embeds the way a `LAYOUT_VERSION` bump re-renders:
   `qmd_index` folds the pin into the version it reports. No runner
   change, no extra step.
7. An embed **runs to completion** by default (`--timeout 0`); the Stop
   button is the lever, `budget_minutes` the opt-in.

## Re-running the experiments

```sh
S=/tmp/qmd-exp; mkdir -p $S/root
tar -xf bazel-bin/tests/fixtures/ingested/qmd_md.tar -C $S/root --strip-components=1
export XDG_CACHE_HOME=$S/root/unified_index/qmd_index XDG_CONFIG_HOME=$XDG_CACHE_HOME NO_COLOR=1
mkdir -p $XDG_CACHE_HOME/qmd && ln -sfn ~/.cache/qmd/models $XDG_CACHE_HOME/qmd/models
qmd() { node bazel-bin/third-party/qmd/runtime/node_modules/@tobilu/qmd/dist/cli/qmd.js "$@"; }
for g in $(ls $S/root | grep -v unified_index); do
  qmd collection add $S/root --name $g --mask "$g/render_markdown/**/*.md"; done
qmd update
qmd embed -c slack                       # finding 3
(qmd embed -c notion &) ; sleep 0.3; qmd embed -c beeper; echo $?   # finding 5: "Skipping", exit 0
(qmd embed &); for i in 1 2 3; do qmd update; done                  # finding 4
sqlite3 $XDG_CACHE_HOME/qmd/index.sqlite \
  "select d.collection, count(distinct d.hash), count(distinct v.hash)
     from documents d left join content_vectors v on v.hash=d.hash
    where d.active=1 group by 1;"
```

Finding 2's direct write is the `UPDATE documents … / INSERT OR IGNORE
INTO content …` pair against that file with `readfile()`, then
`qmd search`, `qmd embed -c`, `qmd update`.
