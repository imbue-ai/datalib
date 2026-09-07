# Streaming steps: the build plan

**Status: plan (2026-09-07). Nothing here is built yet.** The design
this implements is [`streaming_steps.md`](streaming_steps.md); read that
first for *why*. This file is the *how*: what already exists, what has
to be written, in what order, and the traps that were measured rather
than guessed.

Every claim below marked "measured" was checked against the tree or
against the Bazel-built doltlite CLI (`bazel-bin/third-party/doltlite/doltlite`,
engine 0.50.3) on 2026-09-07. The measurements changed two of the
design's conclusions, both noted where they come up.

## The goal, restated

A step commits its output in chunks instead of once at the end. Each
commit is announced. The scheduler hands the announcement to the
consumers of that step, which start working on the new part
immediately, commit and announce in turn, and so on down the chain — so
rows reach the grid while the download that produced them is still
running. The applet serving the grid hears the same announcement and
refreshes.

Two things stay true throughout, and they are what keep this cheap:

- **A missed notification makes a consumer slow, not wrong.** A
  checkpoint is only ever a hint that lets a consumer do less work. A
  consumer that ignores every checkpoint does one pass over one pinned
  view, which is exactly what every step does today.
- **A streaming pass is an ordinary step invocation, dispatched early.**
  No long-lived worker, no stdin channel, no re-pinning framework. This
  is the biggest departure from the design doc and the main reason the
  change stays small; see [The scheduler](#the-scheduler-streaming-dispatch).

## What is already built

More than the design doc suggests. The "what changed upstream since I
last looked" machinery exists and is in production use on **both** edges
we want to stream.

| piece | where | state |
|---|---|---|
| commit-diff scan from a stored cursor to HEAD | [`doltlite_raw.rs::scan_buckets`](../../datalib/backend/etl/src/doltlite_raw.rs) | built, shared |
| render consuming the raw store that way | `providers/{slack,chatgpt,claude,signal,email}/src/render/parse.rs` | built, 5 providers |
| durable render offset | [`render_cursor.rs`](../../datalib/backend/etl/src/render_cursor.rs) (`_render_cursor.json`) | built |
| `grid_index` consuming render stores that way | [`grid_index.rs`](../../datalib/backend/etl/src/grid_index.rs) | built |
| durable per-source index offset | `source_cursors` table in the grid store | built |
| a step reporting a content version per output | [`step_protocol.md`](step_protocol.md), `Event`/`outcome` | built |
| the raw store's version is already a commit-hash pair | `download.rs::raw_store_version` → `entities:<h> blobs:<h>` | built |
| partial output recorded on failure | `StepError::outputs` → `state.output_versions` | built |
| an interrupt-time commit hook per store | `CheckpointSink`, `RawStoreSession::checkpoint_hook` | built |
| a push channel to the UI | `GET /api/sync/stream` (SSE) + `ui/src/live.ts` | built |

So the "consumer contract" the design doc lists as work item 4 is
substantially done, and the commit hook we need for work item 1 already
exists — it just only fires on SIGINT.

## What is actually missing

1. **Producers commit once.** `RawStoreSession::finish` on the download
   side, one `store.commit()` at [`render.rs`](../../datalib/backend/datalib_step/src/render.rs)
   for a whole render. Nothing commits on a timer.
2. **There is no notification.** Nothing on the event stream says "I
   committed"; `outcome` is terminal and last-one-wins in
   [`subprocess.rs`](../../datalib/backend/dag/src/subprocess.rs).
3. **The scheduler runs each step exactly once per run**, and only after
   every dependency has reached a terminal state.
4. **Consumers read content from the working set.** This is the one that
   costs real work, and the next section is about it.

## The hazard: consumers read uncommitted rows

The diffs are safe. Every `scan_buckets` query is
`WHERE from_ref = ?1 AND to_ref = 'HEAD'`, which is committed state
only — measured, including that a row inserted after the last commit is
invisible to it, and that an arbitrary multi-commit range works (the
`from_ref`/`to_ref` hidden-column form is *not* the adjacent-pairs trap
described in `hack/doltlite_concurrent_reader/README.md`; that trap is
the `from_commit`/`to_commit` columns).

The *content* reads are not safe. Once a consumer knows which buckets
moved, it reads them with a plain `SELECT`, which reads the working set
— rows the producer has written and not yet committed. Measured against
one file at one instant:

```
SELECT count(*) FROM t                  → 4    (2 committed + 2 not)
SELECT count(*) FROM dolt_at_t('HEAD')  → 2
```

Relax the scheduler's edges without fixing this and every consumer gets
a **torn** view: part of one commit mixed with part of a batch still
being written. Silently, and only under load.

### There is no connection-level pin

Worth stating because it is the obvious thing to reach for and it would
have collapsed this whole section to one line. Measured:

- doltlite reads exactly one URI parameter, `doltlite_engine`. There is
  no `ref=` / `as_of=` / `at=`.
- `dolt_connect_branch` takes a **branch name**; a commit hash is
  `branch not found`. Creating a branch would be a write, which a
  reader must not do.
- Opening the file `-readonly` does not help: a plain `SELECT` still
  returns the working set.

So the pin has to go into each query, as `dolt_at_<table>('<hash>')`.

### Two properties of `dolt_at_` that shape the helper

Both measured:

- **It composes normally.** It works with aliases, in `JOIN`s, under
  `WHERE`, and its argument is evaluated at runtime rather than needing
  to be a parse-time literal (a subquery works). The bind-parameter form
  should therefore work too — confirm it through sqlx in the first
  patch rather than assuming it.
- **It does not exist for a table that has never been committed.**
  `SELECT … FROM dolt_at_fresh('HEAD')` on a brand-new table is
  `no such table: dolt_at_fresh`, not an empty result. So the helper
  must fall back to the bare table name when there is no pin — which is
  the same cold-start fallback `scan_buckets` already has, and the same
  branch that keeps first runs working.

### How the sites get swept

The two edges are wildly different in size, which is why the order of
work below starts where it does.

`render → grid_index` is **three** sites, all in
[`indexed_markdown.rs::documents_matching`](../../datalib/backend/etl/src/indexed_markdown.rs)
— `markdowns`, `grid_rows`, `edges` — in one shared file.

`download → render` is **40**, across 10 provider crates: slack 8,
beeper 6, yolink 5, whatsapp 5, signal 5, email 5, chatgpt 3,
sms_backup_restore 1, google_takeout 1, claude 1. Measured over every
`providers/*/src/render*` file (some providers have a `render/`
directory and some a single `render.rs` — a grep over only the
directories undercounts), excluding `dolt_*`/`pragma_*`/`sqlite_*`.
Attachment bytes ride along: [`blob_cas.rs::BlobBundle::load`](../../datalib/backend/etl/src/blob_cas.rs)
is two more sites in one shared file, on this edge rather than the
other, and its per-provider projection SQL is among the 40.

A 40-site mechanical sweep where one miss is a silent tearing bug needs
an enforceable shape, not care. Proposal: one template helper on the
store handle,

```rust
// before
sqlx::query("SELECT id, json(payload) AS payload FROM users")
// after
sqlx::query(sqlx::AssertSqlSafe(store.sql("SELECT id, json(payload) AS payload FROM {users}")))
```

where `sql()` substitutes `{users}` → `dolt_at_users('<pin>')` when the
store is pinned and → `users` when it is not. The `AssertSqlSafe`
justification is the one AGENTS.md already blesses: a static template
plus a table name that is `&'static str` at every callsite, and a hash
we minted ourselves.

Then a repo-hygiene check in `scripts/lint_repo.py` fails on any
`FROM <bare-content-table>` under `providers/*/src/render/` — turning
"did we get all 38?" from a review question into a build failure. That
lint is the deliverable that makes the sweep safe; write it before the
sweep, not after.

## The protocol

One new event, and nothing else:

```json
{"event":"checkpoint","step":"slack/rendered_md","version":"<dolt commit hash>"}
```

No `path`, because since `step_identity` shipped a step has exactly one
output and its id *is* that path. No `failure`, because a checkpoint is
never terminal.

A **distinct event rather than an early `outcome`** — the design doc
proposes reusing `outcome`, but `outcome` also carries `failure` and is
handled as last-one-wins in `subprocess.rs`. A new variant is ~20 lines
and leaves no ambiguity about which line ends the step.

Nothing is sent *to* a running step. The scheduler's reply to a
checkpoint is to dispatch the consumer again, which is the design's
"fallback" path (Bazel's `--strategy=worker,local`) promoted to being
the only path.

## Producer side: chunked commits

The seam already exists in both places:

- **Download.** `RawStoreSession` already owns a commit hook it fires on
  SIGINT ([`raw_store.rs`](../../datalib/backend/etl/src/raw_store.rs)).
  Give it a `maybe_checkpoint()` the provider calls at its natural batch
  boundary; it commits and returns the hash when the budget is spent and
  does nothing otherwise. The entities store and the blob CAS are
  separate files with separate HEADs, and the reported version is
  already the pair `entities:<h> blobs:<h>` — so both commit, and the
  checkpoint carries the pair, exactly as the final outcome does.
- **Render.** The `put_document` callback at
  [`render.rs:85`](../../datalib/backend/datalib_step/src/render.rs) is
  the boundary; `IndexedMarkdownStore::commit` is already public.

### Cadence

A shared `Checkpointer` in `etl`: commit when either ~15s has elapsed or
~5k rows/~64MB have been written since the last commit, whichever comes
first, plus an explicit "done for now" call. Defaults in code, override
by env (`DATALIB_DAG_CHECKPOINT_SECS` and friends) rather than by config
file — whether a step can cope with chunked commits is a fact about the
step, not a preference the user has, and the same argument applies to
how often. The env override exists for experiments and for the tests.

The cost of getting this wrong is `dolt_log` size. The existing
one-commit-per-render rule is there precisely because "per-document
commits would put thousands of entries in `dolt_log` per run"
([render.rs](../../datalib/backend/datalib_step/src/render.rs)). Chunking
is the compromise and the chunk size is the dial; a 20-minute download at
15s granularity adds ~80 commits, which is fine, and a per-row commit
would not be.

### The rule that is easy to get wrong

**A checkpoint may only land where the store is semantically
consistent.** Two known places where it must not:

- The **reconcile** path in `grid_index.rs` drops and rebuilds every
  index table together (see the comment on `index_ddl`). A commit
  inside that publishes an empty index as HEAD.
- The **prune-to-snapshot** path on the export-shaped ingests (the
  `claude_export` edge described in AGENTS.md) deletes rows the current
  snapshot does not contain. A commit mid-prune publishes a store that
  is missing data it will have again a second later.

Both are handled the same way: `Checkpointer` is asked, never
self-firing, and the caller does not ask inside a critical section. The
tests for this should assert that a reader pinned at any commit the
producer published sees a store that satisfies the same invariants a
finished run's store does.

## Consumer side

Almost nothing new. On start a consumer already reads its cursor,
already asks `scan_buckets` for the changed set, and already gets
`new_head` back — and `new_head` is exactly the pin its content reads
should use. Today it is only used to stamp the cursor.

So the change per consumer is: thread `new_head` into the content reads
through the `{table}` helper, and advance the cursor to the *same* hash
it read at. Cold start (no pin, or a store with no commits) keeps
today's behavior.

The re-pinning discipline the design doc worries about — a long-lived
consumer serving stale data from a pin it forgot to refresh — does not
arise, because a streaming pass is a fresh process that pins once and
exits.

## The scheduler: streaming dispatch

The smallest change that delivers the whole thing.

Today [`scheduler.rs`](../../datalib/backend/dag/src/scheduler.rs) keeps
`remaining_deps[i]`, pushes `i` onto `ready` when it hits zero, and sets
`status[i]` exactly once. The change:

1. **The graph learns which edges stream.** A `streaming: Vec<bool>`
   parallel to the existing edge data. A step announces the capability;
   it is not config. The cheapest announcement that works: a
   `--capabilities` probe the runner calls once per distinct command and
   caches for the run. (`datalib_step` already has a `probe` subcommand
   to model it on.)
2. **A checkpoint reaches the loop.** `run_subprocess` currently returns
   only at exit, so it needs an `mpsc::Sender` alongside the event sink.
   The loop selects over `set.join_next()` and the checkpoint channel.
3. **A checkpoint may dispatch a consumer early.** On a checkpoint for
   producer `p`: record the new version, then for each consumer `c`
   where the edge streams, `c` is not already in flight, and `c` has no
   terminal status — push `c` onto `ready` even though
   `remaining_deps[c] > 0`.
4. **An early pass is not terminal.** It records its outcome versions
   and its `input_versions` normally, but does not set `status[c]` and
   does not `release_dependents`. The step stays "running" for the whole
   window, which is also the right UI story.

The pleasing part is what falls out. Because an early pass records
`input_versions` the ordinary way, the final pass — dispatched when
`remaining_deps[c]` really does hit zero — hits the existing staleness
predicate and is **skipped** when nothing moved after the last
checkpoint. Streaming is not a second code path beside the scheduler's
rules; it is the same rules, evaluated earlier.

Two invariants to hold explicitly, both cheap:

- **At most one instance of a step in flight.** Single-writer-per-file
  is load-bearing everywhere in this repo, and `grid_index` is a fan-in
  that every source's checkpoint will poke.
- **A streaming pass must not starve a producer.** It occupies a
  parallelism slot. Simplest fix: when popping from `ready`, prefer a
  step whose deps are satisfied over a speculative one.

## The UI

The channel already exists: the worker relays runner events onto
`GET /api/sync/stream`, and `ui/src/live.ts` is the shared client. Add
the `checkpoint` frame to the relay, and have the grid refetch when the
step named is `unified_index/grid`. This is the smallest piece of the
whole change and should be the visible proof it works.

## Order of work

Each of these is a reviewable PR that leaves the tree green.

1. **The lint and the helper.** `store.sql("… FROM {t}")`, the
   cold-start fallback, and the `lint_repo.py` check that fails on a
   bare content-table read in render code. No behavior change; the sweep
   becomes mechanical and enforced.
2. **The sweep.** 3 sites in `indexed_markdown.rs` first, then the 40
   provider sites + 2 in `blob_cas.rs`, all still passing `None` for the
   pin. Still no behavior change — this is the patch to review carefully
   and the one that is boring on purpose. Splitting it in two along the
   edge boundary keeps the first streaming edge unblocked by the wide
   half.
3. **Producer checkpoints.** `Checkpointer`, the two commit seams, the
   `checkpoint` event, `subprocess.rs` parsing it, `progress_bus.rs`
   showing it. Consumers still only run at the end, so this ships
   durability and "N rows committed so far" progress with no scheduling
   risk. Answers most of #164 on its own.
4. **Consumers pin.** Thread `new_head` through. Still no early
   dispatch — but now provably safe against one, and the tests can
   assert it by running a consumer against a store with a dirty working
   set and checking it sees the committed count.
5. **Streaming dispatch.** The scheduler change, `render → grid_index`
   only. Measure the latency change before widening.
6. **`download → render`.** Turn the capability on for the second edge.
7. **The UI frame.**

Steps 1–4 carry no scheduling risk at all, and 3 is independently
useful, so if this stalls partway it stalls somewhere useful.

## What this costs

Roughly **+700 to +800 net lines** for the whole chain, most of it in
steps 1–2 (the sweep is wide and shallow) and in tests. It is not
net-neutral, and it would be dishonest to plan as though it were. The
parts that would normally be expensive — cursors, offsets, incremental
consumers — are the parts already built; what is left is mostly the
mechanical cost of pinning forty-odd queries and the lint that keeps
them pinned.

The first streaming edge on its own — steps 1, 3, 4, 5 with only the
three-site half of step 2 — is roughly **+450**, and is the number to
judge the idea by before committing to the wide half.

## Relation to the linked issues

- **#247 (independent sources sync and stop independently).** The
  scheduler change in step 5 is "the dispatch loop accepts work whose
  dependencies are not yet satisfied, and a step's terminal status is
  deferred." That is structurally what #247's daemon needs in order to
  extend a run in flight. Doing streaming first makes #247's smaller
  version nearly free; the ordering is deliberate.
- **#164 (follow per-step progress, cancel individually).** Step 3
  gives the progress half: a checkpoint is a real, durable "this much is
  committed" that the bus can show. The per-step cancel half is #247's
  daemon, not this.

## Open questions

- **How a step announces streaming.** A `--capabilities` probe is
  proposed above because there is a `probe` subcommand to model it on
  and because it keeps the fact out of the config file. A manifest or a
  field on the first event would also work. Settle it in step 5, not
  before.
- **Whether `dolt_at_` takes a bound parameter through sqlx**, or
  whether the hash has to be interpolated into the template. Measured
  far enough to be confident it will (the argument is evaluated at
  runtime), not far enough to promise it. First thing to check in step 1.
- **What the task board shows** when a step and its consumer are both
  running. The design doc raises this and it is still open; "running,
  with a committed-so-far count" is the obvious answer and step 3 makes
  it available before step 5 needs it.
