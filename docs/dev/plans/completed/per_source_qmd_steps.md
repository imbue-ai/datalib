# Plan: per-source `qmd_embed` steps

**Status: built, 2026-09-15**, in the slices at the end — after one
reversal, recorded in §"The design". Kept as the record of what was
measured and decided; the §"The design" sections describe the tree as
it landed, and the follow-ups under "Slices" are the parts that did
not. Every measurement below was taken
on 2026-09-15 against qmd 2.8.3 (the pin in `runtime/src/qmd.rs`) on a
mac; the recipe is at the end so it can be re-run. "Today" in
§"What the experiments established" means the tree *before* this
landed.

## The ask

Today the qmd index is one global step, `unified_index/qmd_index`, that
fans in every source's `render_markdown` tree and runs `qmd update` then
`qmd embed` over all of it (`datalib_step/src/qmd_index.rs`,
`qmd_indexer/src/lib.rs::run_index`). Since per-source collections
landed, the *index* already has a per-source shape — one qmd collection
per group — but the *work* is still one step: a giant source's embedding
holds every other source's search freshness hostage, and there is no way
to run, stop or resume one source's indexing on its own.

The change, as it landed: each source gets its own `qmd_embed` step,
so the slow part — embedding — is runnable, stoppable and resumable per
source. The keyword index stays one `qmd_index` fan-in over every
source (`qmd update`, which rescans every collection and is fast), and
`grid_index` is untouched. The qmd store stays one SQLite file, and one
`qmd mcp` daemon keeps serving every collection from it.

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

9. **`qmd collection add` indexes the tree as it registers it**
   (`collectionAdd` → `indexFiles`, `qmd.ts:1851`), and a collection is
   a YAML entry in `<XDG_CONFIG_HOME>/qmd/index.yml` — with the data
   root's *absolute* path — that qmd syncs into `store_collections` on
   every start. So registering a new source costs one qmd-side scan
   at `collection add` and a second at the `qmd update` that follows;
   the second finds every row unchanged. (The backed-out per-source
   writer's test pinned that from the other direction: rows it wrote
   were `unchanged` to `qmd update` too.)

## The design

### One keyword index, one embedding step per source

```
<g>/ingest  →  <g>/render_markdown  →  unified_index/qmd_index  →  <g>/qmd_embed
                       │                         ▲
                       └──→  unified_index/grid_index   (unchanged)
                                (every source's render step feeds both fan-ins)
```

- **`unified_index/qmd_index`** (unchanged in shape): register one qmd
  collection per group, `qmd update` over all of them, retire the
  collections no group claims any more. It no longer pulls models or
  embeds — those moved to the per-source step. New: it **reports a
  version**, a digest over every active document's `(collection,
  path, hash)` plus the qmd pin (`store::index_version`). Without one
  the runner would content-hash its tree for every consumer — the
  whole `index.sqlite`, and through the `models` symlink two gigabytes
  of GGUF.
- **`<g>/qmd_embed`** (new, optional per source): `inputs =
  ["unified_index/qmd_index"]`, `lock = "qmd_embed"`. Runs `qmd embed
  -c <g>` in a loop until qmd reports nothing pending, or until
  `params.budget_minutes` runs out. A source without this step is
  keyword-searchable and not semantically searchable, which is what
  the grid's `Embedded` column already knows how to show.

Because the fan-in's version moves whenever any source's documents
change, every embed step goes stale together and each runs; one whose
collection has nothing pending spawns qmd once (~0.7s of node startup)
and stops. That is the cost of not knowing qmd's embedding fingerprint
in Rust — see below — and it is cheap.

**What was tried first, and backed out.** The first cut made the
keyword index per source too — `<g>/qmd_index` — because `qmd update`
cannot be scoped to a collection (finding 1). Since it cannot, that
step had to write qmd's `documents`/`content` rows itself: a port of
qmd's `reindexCollection` (finding 2 showed qmd could not tell the
difference). It worked, and it was a third of the change: a writer
for another program's tables, a test proving byte-identity with that
program, the loader rejecting the old fan-in and the migrator
rewriting it, two extra step blocks per source in every config, and an
"index" phase per source in the UI. Rescanning every tree on every
sync is fast, so it was accepted instead, and the writer went. What
this costs: `qmd update` reads and hashes every rendered file each
sync — O(corpus), no mtime shortcut — the thing to watch on a big
root.

**The shared file.** Both steps write `index.sqlite`; each source's
`<g>/qmd_embed/` is an empty directory, created so the tree the id
names exists to be measured. (A first cut left a `state.json` there;
nothing read it, so it went.)

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

`qmd_index` inherits that: its inputs move every sync, so it runs every
sync, and it folds the qmd pin into the version it reports. A qmd bump
moves the version, every `qmd_embed` is stale, each runs, and each
finds pending > 0 — qmd computes its own embedding fingerprint
(`store.ts:114`: model name plus chunking and formatting constants,
all of which change only with the qmd version) and embeds. A re-render
is the same story one level up: the bytes change, the hashes change,
the documents are pending.

`qmd pull` — previously run on every index pass, a network round-trip
per sync — moved into `qmd_embed`, gated on `models_present`, under the
embed lock so two sources never pull at once. It fetches the
query-expansion and reranker models as well as the embedding one,
which is what search needs, so it stays a pull of all three.

### Three locks around embedding, each for a different failure

Findings 4 and 5: `qmd update` may run beside one `qmd embed`, but two
embeds must not overlap, and qmd's own refusal is silent. Three layers,
innermost first:

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

A source's TOML grows one step, and the fan-ins are as before:

```toml
[[steps]]
group = "slack"
function = "qmd_embed"
inputs = ["unified_index/qmd_index"]
lock = "qmd_embed"
```

- **Wizard** (`SourceWizard.vue`, `sourceSteps.ts`): writes it under a
  "Semantic search" tick under Rendering, on by default, disabled when
  the config has no `unified_index/qmd_index` for it to read. Editing
  replaces every step the source owns in one cut, so unticking removes
  it. The fan-in wiring (`wireIntoFanIns`) is unchanged.
- **Manage screen**: rows are assembled server-side (`GET
  /api/manage/rows`); the embed step is phase `embed`, labelled
  "Embeddings", with its own glyph. Deleting a step takes everything
  that reads it, across groups — so deleting the shared qmd index takes
  every source's embedding step. "Sync now" on an embed row resumes it:
  `--sync` accepts any step id, and the subgraph is the step and its
  dependents.
- **`datalib-migrate-config`**: a config with the `qmd_index` fan-in
  and no `qmd_embed` step anywhere is the shape from before embedding
  was its own step, and the fan-in used to embed everything it named;
  the rewrite gives each source it names an embed step, so semantic
  search keeps covering what it covered.
- **Scaffold** (`http/src/lib.rs::scaffold_toml`), the three
  `docs/user/config_examples/`, `configs/dag_example.toml`, the docker
  demo, `agent_config_guide.md`, `first_time_user.md`: same shape.

### What lands where

| piece | crate |
|---|---|
| `store::index_version`, `store::embed_gauge` (read-only over qmd's file); `embed_group` (loop, budget, lock detection, gauge thread) | `qmd_indexer` (gains sqlx, sha2, libc and tokio — third-party, pinned; the BUILD comment says why that is fine) |
| `Function::QmdEmbed`, `qmd_embed.rs`; `qmd_index` reports a version and no longer pulls or embeds | `datalib_step` |
| `lock` key, `FailureKind::Incomplete` / `RunState::Incomplete`, dispatch gate, `--sync` any step, `biased;` on the checkpoint select (#462) | `dag` |
| `--embed-group` on the CLI, through `build_qmd_index.py` | `qmd_indexer/src/main.rs`, `tests/fixtures/` |
| `Phase::Embed`, labels, `incomplete` status | `http/src/manage/` |
| wizard tick, `embedStepToml`, delete cascade, `DagRunState` | `ui` |
| the `NoEmbedSteps` rewrite | `migrate_config` |

## Slices

1. **Library** — *built*. `embed_group` (loop, budget, lock, gauge,
   `src/embed.rs`), `store::index_version` and `store::embed_gauge`
   (`src/store.rs`), with `tests/embed_group.rs` pinning the scoping of
   `embed -c`, the exact busy line qmd prints, that the loop sees
   through its exit 0, and that the index version moves with the
   documents and the pin and only then. `run_index` composes them for
   the fixture and the global step; the CLI takes `--embed-group`.
2. **Runner** — *built*. `lock` on `[[steps]]` and `StepSpec`, the
   dispatch gate (a waiter goes back to the front of the queue when the
   holder lands, holding no slot meanwhile), `FailureKind::Incomplete`
   → `RunState::Incomplete`, no retry, dependents blocked, the run's
   exit code unaffected; `--sync` accepts any step id. Both halves of
   the vocabulary (`DagRunState`, the status labels, the DAG view) and
   `step_protocol.md`.
3. **Steps** — *built*. `qmd_embed` in `datalib-step`; `qmd_index`
   reports a version and leaves models and vectors to the embed steps.
   The migrator adds embed steps to a config that has none. Every
   example config, the scaffold, the docker demo (its whole search
   index below the build-time cut), the agent guide and the user guide
   carry the new shape. Run end to end through `datalib-dag` over the
   fixture's markdown: a 3-second budget ended `incomplete`, the next
   run resumed and succeeded, the run after that skipped every step as
   up to date.
4. **UI** — *built*. The wizard's "Semantic search" tick, on by
   default; the Manage screen's "Embeddings" rows; deleting a step
   takes everything that reads it.
5. **Fixture** — *built*. `tests/fixtures/qmd_groups.bzl` names the two
   groups (`slack`, `claude-api`); the genrule passes them as
   `--embed-group`, and the tests that assumed everything was embedded
   now assert the line exactly — a document is embedded iff its group
   is one of the two, which is the first time the `Embedded` column has
   been tested as a column of its own.
6. **Follow-ups**, deliberately out of scope: the daemon respawn above;
   a "Re-embed" row action (`qmd embed -f -c`); cancelling one step
   without the run; a per-source keyword index, if `qmd update`'s
   full rescan ever becomes the slow part (the first cut of this plan,
   in git, is how — and `qmd update -c` upstream would be the ten-line
   version).

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
8. The keyword index stays one `qmd update` over every collection —
   the per-source writer of qmd's tables was built, measured, and
   backed out for the third of the change it cost.
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
