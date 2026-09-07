# Streaming steps: the build plan

**Status: agreed plan (2026-09-07), being built.** The design this
implements is [`streaming_steps.md`](streaming_steps.md); read that
first for *why*. This file is the *how*: what already exists, what has
to be written, in what order, and the traps that were measured rather
than guessed. [Order of work](#order-of-work) is the checklist; update
it as slices land, and treat anything it still lists as unbuilt.

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
  The one thing this does *not* get for free: **a step must never be
  dispatched while an earlier pass of it is still running.** Every store
  in this tree has one writer, and a fan-in like `grid_index` will be
  poked by every source's checkpoints, so the second poke will land
  while the first pass is still going. The scheduler has to track
  in-flight per step and drop (not queue) a checkpoint for a step
  already running — dropping is safe because the next checkpoint, or
  the final pass, subsumes it.

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

So the commit has to be named in SQL, as `dolt_at_<table>('<hash>')`.

Not per query, though — per *connection*. A `TEMP VIEW` over that
expression is a pure read that leaves the file untouched, and views are
per-connection, so one pass installs a set of them once and every query
afterwards reads committed state. That is what `install_views` does, and
it is why the pin never has to be threaded through the code that runs
the queries. It also keeps the entities/CAS case working, since those
are two files with two pools and therefore two independent view sets.

### Two properties of `dolt_at_` that shape the helper

Both measured:

- **It composes normally.** It works with aliases, in `JOIN`s and under
  `WHERE`, and through a view. `pin.rs`'s
  `pinned_views_read_the_commit_not_the_working_set` proves it end to end
  through sqlx rather than through the shell: against a store with a
  dirty working set the plain read sees two rows and the pinned view sees
  the one that was committed, including across a join between two pinned
  views. That test is the premise of this entire plan, so it was checked
  against a deliberate break to confirm it can fail.
- **It does not exist for a table that has never been committed.**
  `SELECT … FROM dolt_at_fresh('HEAD')` on a brand-new table is
  `no such table: dolt_at_fresh`, not an empty result.

  To be clear about why that matters, because "read a table nobody has
  committed" is not a thing we should ever want to do: the point is not
  that we process uncommitted tables, it is that **the pin can
  legitimately be absent**, and when it is, the pinned form *crashes*
  rather than returning nothing. Two ways it goes absent today, both
  already handled by `scan_buckets`'s cold-start branch:

  - a **dev build against stock libsqlite3**, where there are no dolt
    extensions at all, so `dolt_log()` does not resolve and `new_head`
    is `None`;
  - a store with **zero commits** — a download that wrote rows and
    whose commit failed, since `commit_with_suffix` is best-effort and
    logs rather than failing the step.

  So the fallback is crash-avoidance on paths that already exist, not a
  feature. And it needs one new restriction under streaming, which is
  the real answer to the question: an unpinned read *is* a working-set
  read, so a consumer with no pin must not read a store while its
  producer is live. It should do nothing that pass and wait for the
  final one, rather than fall back to reading torn rows.

### A second way to tear: one download, two stores

A download writes **two** doltlite files with two independent HEADs —
`raw/entities.doltlite_db` and `raw/blobs.doltlite_db` — and render
reads both (`BlobBundle::load` takes a `refs_pool` and a `cas_pool`).
So even with every query pinned there is a tear available between the
two pins, and the two commits have to be ordered.

**Commit blobs first, then entities.** The reference direction settles
it. An entity row names a blob by its blake3, so:

- entities first → a consumer can pin (entities=new, blobs=old) and see
  a row referencing bytes that are not in its CAS pin. A dangling
  attachment, which is a real defect.
- blobs first → the worst available pin is (entities=old, blobs=new):
  bytes in the CAS that nothing references yet. Harmless, and already
  the normal state — the CAS is content-addressed and written with
  `INSERT OR IGNORE`, so unreferenced blobs are routine.

**No tagging scheme is needed to pair them**, because the pair is
already the version string. `download.rs::raw_store_version` reports
`entities:<h> blobs:<h>`, so a checkpoint carries both hashes in one
value the consumer splits — which is exactly what a matching tag would
have bought, minus the tag. The rule for the consumer is that both
pins come out of *one* checkpoint string, never sampled separately.

### And a third: `to_ref = 'HEAD'` is a moving target

`scan_buckets` samples `new_head` from `dolt_log()` and then runs the
bucket query with `to_ref = 'HEAD'` — a *symbolic* ref, resolved when
the query runs. Nothing writes concurrently today, so the two always
agree. Under streaming they can differ: a producer that commits between
those two statements gives you a changed-bucket list computed at a
newer commit than the pin the content reads will use, so the consumer
looks for rows its pin does not have.

The fix is one line and belongs in the same patch as the pinning:
**`scan_buckets` must diff to the literal `new_head` hash, not to
`'HEAD'`.** The scan and the content reads then name the same commit by
construction. This answers "does the helper work for the renderer's
initial work-to-do diff too?" — yes, and the pin it hands the content
reads has to be the same hash the diff was taken at, which is why
`new_head` (already computed, today only used to stamp the cursor)
becomes the pin rather than a freshly sampled HEAD.

### How the sites get swept

The two edges are wildly different in size, which is why the order of
work below starts where it does.

`render → grid_index` is **three** sites, all in
[`indexed_markdown.rs::documents_matching`](../../datalib/backend/etl/src/indexed_markdown.rs)
— `markdowns`, `grid_rows`, `edges` — in one shared file.

`download → render` is **48**, across 10 provider crates: slack 9,
email 8, whatsapp 7, beeper 6, signal 6, yolink 5, chatgpt 4, claude 1,
google_takeout 1, sms_backup_restore 1. Attachment bytes ride along:
[`blob_cas.rs::BlobBundle::load`](../../datalib/backend/etl/src/blob_cas.rs)
is two more sites in one shared file, on this edge rather than the
other, and its per-provider projection SQL is among the 48.

That number was 40 until the lint below was written and counted for
itself, which is the argument for writing the lint first in miniature.
Two things a hand count missed:

- **`JOIN <table>` is a content read too**, and eight of the sites are
  joins rather than `FROM`s.
- **Several of those joins are inside the `dolt_diff_*` bucket queries**
  — `slack/parse.rs:213` joins live `messages` against the diff,
  `email/parse.rs:285` joins live `emails`. So the "already safe" scan
  path has unpinned reads sitting in the middle of it, which is exactly
  the kind of thing a mechanical check finds and a careful reader does
  not.

A 48-site sweep where one miss is a silent tearing bug needs an
enforceable shape, not care. Both halves of that are now built.

**The helper** is [`datalib_etl::pin`](../../datalib/backend/etl/src/pin.rs).
`install_views` creates one `pinned_<table>` view per table over
`dolt_at_<table>('<hash>')`, once per connection, and a query reads the
view:

```rust
// before
sqlx::query("SELECT id, json(payload) AS payload FROM users")
// after
sqlx::query("SELECT id, json(payload) AS payload FROM pinned_users")
```

**The views are named distinctly rather than shadowing the tables**, and
that is the whole design. A temp view named `users` shadows the real
`users`, which would pin every existing query with no edit at all — a
zero-site sweep. It was tempting and it is wrong, because a pass that
forgot to install the views would then *silently* read the working set.
The distinct name turns that into `no such table: pinned_users`. It also
leaves writes through the real names working, so a pool that reads and
writes is unaffected and no audit of which pools do both is needed.

The sweep is therefore still ~50 sites, but each is a one-token rename
in a string literal with no signature change. Note what it is *not*: an
earlier draft had queries call `pin.sql("… FROM {users}")`, which would
have meant threading a `&Pin` down to every function that runs a query
across ten crates. The views hold the pin on the connection instead, so
query text stays `&'static str` and the pin is known only to the code
that opens the store.

Four properties, all measured (`pin.rs`'s `view_tests` guard the first
three, and each was checked against a deliberate break):

- **A missing view fails loudly** rather than reading the working set —
  `a_missing_pinned_view_fails_loudly`.
- **Pinned reads ignore a dirty working set**, including across a join
  between two pinned views —
  `pinned_views_read_the_commit_not_the_working_set`.
- **The views are connection-scoped.** Installing them once covers every
  later query on that pool, and a second connection to the same file
  does not inherit them (it gets `no such table`, not a silent
  working-set read). Our pools are already size 1 with recycling
  disabled — `doltlite_raw`'s `open_disables_connection_recycling`,
  which exists because doltlite's own session state is per-connection —
  so this reuses an invariant the tree already guards rather than adding
  one.
- **Creating them writes nothing**: the file is byte-identical
  afterwards, no commit, no branch.

Two details that only showed up in the building:

- **A table with no `dolt_at_` module** — one created after the last
  commit — gets a view of `SELECT * FROM main.<t> WHERE 0`. Right
  columns, no rows, which is the honest answer: there is no committed
  state, and the uncommitted rows are not ours to read.
- **The hash is interpolated, not bound.** `Pin::at` validates it as 40
  lowercase hex at construction — exactly what the engine accepts, since
  it rejects even a shortened prefix with `ref not found` — so the
  interpolated value cannot be anything else, and the `AssertSqlSafe` is
  asserted in one place instead of at every callsite.

`Pin` **cannot hold `HEAD`** — only a full hash. That makes the
`to_ref = 'HEAD'` race below unrepresentable rather than merely
discouraged, which is most of what the type is for.

**The lint** is check 4 in `scripts/lint_repo.py`, and it is a
**ratchet with a baseline** rather than a hard zero — which is what lets
it land before the sweep instead of after. It fails in both directions:
a new unpinned read added, or an existing one fixed without moving the
baseline down. A converted site is invisible to it, because
`pinned_users` is on the allowed-prefix list next to `dolt_`.

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

A shared `Checkpointer` in `etl` the producer *asks* at its natural
batch boundary. Three refinements over the obvious fixed interval, all
from review:

**It is a debounce with a ceiling, not a period.** Commit when writes
have been quiet for `debounce` (default ~2s), or when `max_interval`
(default ~15s) has passed since the last commit, whichever comes first.
A fixed period makes a source that finishes a burst sit on its rows for
the rest of the interval; a pure debounce never fires under a steady
stream. Debounce-with-ceiling gets the good half of each: a burst that
ends is published promptly, and a continuous writer still publishes
every `max_interval`.

**Nothing changed means no commit.** Check `dolt_status` before
committing and skip when the store is clean, and emit no `checkpoint`
event when `dolt_commit` returns `None` (which it already does for an
empty commit — `commit_with_suffix` has the arm for it). Without this a
long quiet stretch fills `dolt_log` with empty commits and wakes every
consumer to discover nothing moved.

**The cadence is the user's to set.** Earlier drafts of this plan put
it behind env vars, arguing that whether a step can cope with chunked
commits is a fact about the step rather than a preference. That is true
of the *capability* and not of the *cadence* — how much latency to
trade for how much `dolt_log` is exactly the kind of call a person
should get to make. So: capability stays step-declared, cadence goes in
`config.toml` as a top-level default with a per-step override, and the
env vars remain only for tests.

The cost of getting the dial wrong is `dolt_log` size. The existing
one-commit-per-render rule is there precisely because "per-document
commits would put thousands of entries in `dolt_log` per run"
([render.rs](../../datalib/backend/datalib_step/src/render.rs)). A
20-minute download at 15s granularity adds ~80 commits, which is fine;
a per-row commit would not be.

### The rule that is easy to get wrong

**A run that wipes and re-ingests must not checkpoint at all.** Not
"must not checkpoint inside the critical section" — the whole run is
the atomic unit, because a partially re-ingested store is wrong to push
downstream at *any* point in it, not just mid-delete. Half a re-ingest
looks exactly like a source that lost most of its data, and every
consumer would faithfully propagate that.

Three cases, and the first two are the same case:

- **`reset_and_redownload`** truncates every data and bookkeeping table
  and re-fetches ([`control.rs`](../../datalib/backend/etl/src/control.rs)).
  Checkpointing is disabled for the whole run when
  `DATALIB_DAG_RESET_AND_REDOWNLOAD` is set.
- **Prune-to-snapshot** on the export-shaped ingests (the
  `claude_export` edge in AGENTS.md) deletes whatever the current
  snapshot does not contain. Same treatment: an ingest that declares
  itself snapshot-shaped does not checkpoint.
- **`grid_index`'s reconcile** drops and rebuilds every index table
  together (see the comment on `index_ddl`), so a commit inside it
  publishes an empty index. This one really is a critical section
  rather than a whole run, and it is enough that `Checkpointer` is
  asked rather than self-firing and the reconcile path does not ask.

The test to write is the general one: a reader pinned at *any* commit
the producer published must see a store satisfying the same invariants
a finished run's store does. If that is hard to state for some ingest
shape, that shape should not checkpoint.

## Consumer side

Almost nothing new. On start a consumer already reads its cursor,
already asks `scan_buckets` for the changed set, and already gets
`new_head` back — and `new_head` is exactly the pin its content reads
should use. Today it is only used to stamp the cursor.

So the change per consumer is: thread `new_head` into the content reads
by installing the pinned views at that hash, and advance the cursor to
the *same* hash it read at. Cold start (no pin, or a store with no commits) keeps
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

Two invariants to hold explicitly:

- **At most one instance of a step in flight.** Single-writer-per-file
  is load-bearing everywhere in this repo, and `grid_index` is a fan-in
  every source's checkpoints will poke, so a second poke arriving
  mid-pass is the common case rather than the rare one. A checkpoint
  for a step already running is **dropped, not queued** — the next
  checkpoint or the final pass subsumes it, which is the
  slow-not-wrong rule doing its job.
- **A streaming pass gets its own slot, outside `parallelism`.** An
  earlier draft said to prefer deps-satisfied steps when popping from
  `ready`, which review caught as self-defeating: with `parallelism`
  at 4 and four downloads running, a strict preference means
  `grid_index` never runs and *nothing reaches the UI* — losing the
  entire point of the change in precisely the case it was meant for.

  So give streaming passes a small separate budget (start at 1)
  rather than making them compete. The justification is that
  `parallelism` exists to bound long, network-bound fetches, and a
  streaming pass is neither — it is bounded incremental work over a
  delta, already capped at one instance per step. If the budget turns
  out to need tuning it should be tuned as its own number, not by
  borrowing from `parallelism`.

## The UI

The channel already exists: the worker relays runner events onto
`GET /api/sync/stream`, and `ui/src/live.ts` is the shared client. Add
the `checkpoint` frame to the relay, and have the grid refetch when the
step named is `unified_index/grid`. This is the smallest piece of the
whole change and should be the visible proof it works.

## Order of work

Each of these is a reviewable PR that leaves the tree green.

1. ~~**The lint and the helper.**~~ **Done.**
   [`etl/src/pin.rs`](../../datalib/backend/etl/src/pin.rs) (`Pin`, the
   `install_views`, the `pinned_<table>` naming, the empty view for a
   table absent at the pin) and check 4 in `scripts/lint_repo.py`,
   holding a baseline of 48. No behavior change: nothing calls
   `install_views` yet. Carries the tests that a pinned view ignores a
   dirty working set, that a missing view fails loudly, and that the
   views are connection-scoped — the three assertions the rest of this
   plan rests on.
2. **The sweep.** 3 sites in `indexed_markdown.rs` first, then the 48
   provider sites + 2 in `blob_cas.rs`, all still `Pin::Unpinned`. Still no behavior change — this is the patch to review carefully
   and the one that is boring on purpose. Splitting it in two along the
   edge boundary keeps the first streaming edge unblocked by the wide
   half.
3. **Producer checkpoints.** `Checkpointer` (debounce + ceiling, skip
   when clean, cadence from config), the two commit seams with **blobs
   committed before entities**, checkpointing disabled for
   wipe-and-re-ingest runs, the `checkpoint` event, `subprocess.rs`
   parsing it, `progress_bus.rs` showing it. Consumers still only run
   at the end, so this ships durability and "N rows committed so far"
   progress with no scheduling risk. Answers most of #164 on its own.
4. **Consumers pin.** Thread `new_head` through, and change
   `scan_buckets` to diff to that literal hash rather than to `'HEAD'`
   so the scan and the content reads name one commit. Still no early
   dispatch — but now provably safe against one, and the tests can
   assert it by running a consumer against a store with a dirty working
   set and checking it sees the committed count.
5. **Streaming dispatch.** The scheduler change — in-flight tracking
   with checkpoints dropped rather than queued, and the separate
   streaming slot — for `render → grid_index` only. Measure the latency
   change before widening.
6. **`download → render`.** Turn the capability on for the second edge.
7. **The UI frame.**

Steps 1–4 carry no scheduling risk at all, and 3 is independently
useful, so if this stalls partway it stalls somewhere useful.

## What this costs

Roughly **+800 to +900 net lines** for the whole chain, most of it in
steps 1–2 (the sweep is wide and shallow) and in tests. It is not
net-neutral, and it would be dishonest to plan as though it were. The
parts that would normally be expensive — cursors, offsets, incremental
consumers — are the parts already built; what is left is mostly the
mechanical cost of pinning forty-odd queries and the lint that keeps
them pinned.

That is up about a hundred lines from the first draft of this plan,
which is what review cost: the debounce-with-ceiling, the config
plumbing for cadence, the entities/CAS commit ordering, the
`dolt_status` skip, and in-flight tracking in the scheduler. All of it
buys correctness or control rather than scope.

The first streaming edge on its own — steps 1, 3, 4, 5 with only the
three-site half of step 2 — is roughly **+500**, and is the number to
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
- **What the task board shows** when a step and its consumer are both
  running. The design doc raises this and it is still open; "running,
  with a committed-so-far count" is the obvious answer and step 3 makes
  it available before step 5 needs it.
- **How big the streaming budget should be.** One slot is the starting
  guess and the argument for it is only that a streaming pass is
  bounded work rather than a fetch. If a root with many sources turns
  out to keep a fan-in permanently behind, the answer is to raise that
  number rather than to reach into `parallelism` — but nobody has
  measured it. Revisit after step 5 has run against a real root.
