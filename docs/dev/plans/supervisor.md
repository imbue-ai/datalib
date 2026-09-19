# The supervisor: steps as managed processes, not as a batch run

**Status: greenfield proposal (2026-09-19), not built.** This is the
alternative to [`join_running_sync.md`](join_running_sync.md), which
patches the runner we have. Both start from the same measurement
(§0 there). This one asks what we would build if the UI's needs came
first. §1 describes the tree as it stands at `ae2d52f0`; nothing else
here describes the tree. Where this doc and the tree disagree, the tree
wins.

## 0. The claim

The DAG runner is a **build tool**: `make -j4` for data. Hand it a
target set, it computes the closure, runs what is stale, writes a
record of the run, exits. That is the right shape for a batch job and
for CI, and it is the shape `datalib-dag <config>` should keep.

It is the wrong shape for a screen with buttons on it. What the Manage
screen is trying to be is **Activity Monitor for a pipeline**: a row per
step, each with a live state, and per-row controls — start this, stop
this, leave this alone for now. Every awkwardness we have hit is the
same mismatch wearing different clothes:

- A sync started during a run waits for the run, because "the run" is
  the unit and it was planned at start.
- Stop is per run, not per row, because one process owns the plan.
- "Queued" is *inferred* by `manage/status.rs` from job rows,
  timestamps and `dag_state.json`, because no component actually
  *holds* that state — the worker knows about jobs, the runner about
  steps, and the row on screen is the join of the two done after the
  fact.
- Streaming needed `in_flight`, `final_pass_owed` and
  `streaming_pass_owed` because "a consumer runs again after it
  finished" has no natural home in a run that is supposed to end.
- Two steps cannot feed one store because a step's id *is* its output
  tree, which was the cheap way to get single-writer.

The proposal: keep the DAG as the description of **dataflow**, and
replace the runner-and-worker pair with one long-lived **supervisor**
that reconciles three things continuously —

| | what it is | who owns it |
|---|---|---|
| **graph** | steps, the sinks they write, the sinks they read | the config |
| **intent** | which steps a person wants running, paused, or run once | the UI, and the CLI |
| **facts** | every sink's current version; every invocation and how it ended | the supervisor's store |

— and starts a step whenever it is *wanted*, *stale*, *not running*,
and *its sink is free*. There is no run. There are invocations, and
there is what the user asked for.

## 1. What the tree already has that this builds on

Checked against the tree. Most of the storage-side work is done.

- **The step protocol is already the right contract.** A step is a
  subprocess that writes under its tree, is idempotent, commits
  atomically, reports content versions on stdout and checkpoints as it
  goes, and takes SIGINT as "checkpoint and exit"
  ([`step_protocol.md`](../step_protocol.md)). Nothing in this design
  changes what a step sees, except one environment variable (§2.6).
- **One writer at a time per file is enforced where it matters.**
  `RawDb::open` takes `flock(2)` on `<store>.doltlite_db.lock` and
  refuses a second writer with the holder named; the kernel releases it
  on death. The etl README says outright that this is per file, not per
  root, "so two sources with nothing in common can be written by two
  runners at once (#247)". The storage layer permits exactly the
  concurrency the scheduler forbids.
- **A reader pins a commit and is correct at any pin.** That is the
  streaming design's whole safety argument: "a missed notification
  must make a consumer slow, not wrong" — doltlite is a log of states,
  so a consumer that reads at any committed version is right
  ([`streaming_steps.md` § The rule](streaming_steps.md)).
- **Versions are content-derived and compared for equality**, and a
  dolt commit hash is the canonical one. A version already belongs to
  the *output*; the scheduler just happens to learn it from the one
  step allowed to produce it.
- **The run store is SQLite and already the UI's source of truth for
  what a step is doing** (`system/runs.sqlite`: `step_runs`, `log`,
  `metrics`; pushed to the UI as `table_changed` frames by `watch.rs`).
  `dag_state.json` is the second record, kept for the scheduler's own
  memory of versions.
- **The http server already watches the config and reloads it live**
  (`app_ready` flips both ways), and already hosts a long-lived loop
  (`worker.rs`).
- **The retry policy, failure kinds, subprocess plumbing and the stdout
  event parser** (`retry`, `FailureKind`, `subprocess.rs`,
  `runs_sink.rs`) are independent of the run loop and carry over.
- **`streaming_steps.md` § Why Bazel's persistent workers are the
  better model** already argued for a resident process fed work over
  stdin. This design is that argument taken one level up: not the step
  resident, the *scheduler* resident.

## 2. The design

### 2.1 Sinks are first-class; a step writes one and reads some

Today the graph is steps with edges between them, and a step's id names
the one tree it writes. The new graph is **bipartite**: steps and sinks.

```toml
[[steps]]
group = "work-email"
function = "ingest_gmail"          # a step id is still <group>/<function>
writes = "work-email/raw"          # default: the step's own id

[[steps]]
group = "work-email"
function = "ingest_mbox_import"
writes = "work-email/raw"          # a second writer of the same sink

[[steps]]
group = "work-email"
function = "render_markdown"
reads = ["work-email/raw"]         # `inputs` today; names a sink now
```

`writes` defaults to the step's id and `reads` is `inputs` renamed, so
every current config is a valid new one. What changes is what the
loader refuses: **two steps may name one sink.** The rule becomes
*one writer at a time*, and the supervisor is what enforces it — it
never starts a second writer on a sink while one runs, and the store's
`flock` is the backstop if something slips past.

Why want it: the `claude` provider's api and export methods share one
store today by being one step with two methods; an mbox import beside
a live Gmail pull is the same wish for email; a backfill step beside an
incremental one is the same wish for anything. And the fan-ins are the
case where it matters most: `grid_index` is one step that indexes every
source, so a slow source's render holds the others' rows out of the
grid. With sinks first-class it could be one indexing step *per
source*, all writing `unified_index/grid_index`, serialized by the sink
and scheduled independently — which is the concurrency the user is
asking for at the fan-in, not only at the roots.

**A sink's version belongs to the sink.** For a doltlite store it is
the head commit, read by the supervisor after any writer finishes
(`datalib_history` already reads a store's log without linking `etl`).
For a tree with no store of its own — a markdown tree, qmd's
`index.sqlite` — the step reports it as it does today, and the last
report stands. The step's `outcome` line keeps working unchanged; it is
just no longer the *only* way a sink's version can move.

### 2.2 Intent: what a person asked for, per step, sticky

Every step has a **mode** the user sets, and a **want** that is one-shot:

| mode | meaning | default for |
|---|---|---|
| `auto` | run whenever stale | steps with `reads` |
| `manual` | run only when wanted | steps with no `reads` (sources) |
| `every: 15m` | run when wanted, and when due | a source the user scheduled |
| `paused` | never start; if running, interrupt | — |

*Want* is set by the **Sync** button (and by `POST /api/steps/<id>/want`,
and by the CLI): "run this once, now". It clears when an invocation of
the step *completes*. Sync on a source therefore means what it does
today — the source runs, then everything downstream becomes stale and
runs because it is `auto` — but the rows downstream are not queued
*into a run*; they are simply stale rows that the supervisor will
reach.

**Stop** interrupts the running invocation (SIGINT, then the grace
already in the protocol) and leaves the mode alone: an `auto` step
stopped while stale will start again on the next tick, which is what
"stop" on a derived step should honestly say — so on `auto` steps the
button reads **Pause**, and does both. **Sync everything** wants every
source. There is no per-run Stop because there is no run; a person who
wants the pipeline quiet pauses the sources, and the derived steps
drain and go idle.

Intent is stored, not inferred: a row in the supervisor's store, so a
reload or a restart picks up where the person left it, and so a paused
source stays paused across app launches — which today has no
representation at all.

### 2.3 The reconcile tick

One loop, one function, run on every event (a step finished, a
checkpoint arrived, intent changed, the config changed, a schedule
came due) and on a slow timer as the fallback:

```
for step in graph, in topological order:
    if running(step):                       continue
    if mode == paused:                      state = paused;          continue
    if not (wanted(step) or (mode == auto and stale(step)) or due(step)):
                                            state = idle;            continue
    if sink_busy(step.writes):              state = waiting(sink);   continue
    if budget_exhausted(step.class):        state = waiting(budget); continue
    start(step, consumed = versions of step.reads right now)
```

`stale(step)` is today's predicate, unchanged in substance: never
succeeded, or some read sink's version differs from what its last
successful invocation consumed, or its own fingerprint changed. The
first clause of today's rule — "no inputs, so always run" — is gone,
replaced by *wanted* or *due*: a source runs because someone asked.

Everything the runner's loop needed special machinery for falls out:

- **A consumer runs while its producer is still running.** A checkpoint
  moves the sink's version; the consumer is stale; the tick starts it
  against the checkpointed commit. When the producer finishes, the sink
  moves once more; if the consumer consumed an earlier version it is
  stale again and runs once more, else it is fresh. No `final_pass_owed`,
  because there is no final pass — only "is it stale now".
- **A source added mid-sync starts now.** The config reload adds a
  step; the Sync click sets its want; the tick starts it. Nothing to
  join.
- **Sync pressed on a running source.** Its want is set; the tick sees
  it running and does nothing; when it finishes, the want is still set
  (an invocation that *started before* the want does not clear it), so
  it runs once more. One more pass, no bookkeeping.
- **At most one instance of a step at a time.** It writes a sink, and
  the sink is busy while it runs.

### 2.4 Budgets, not a slot count

Today `parallelism = 4` bounds everything alike and a streaming budget
sits beside it because four downloads starved the index. Steps are
classed — `network` for ingests, `cpu` for renders, `index` for the
fan-ins — with a budget per class, defaulted by function and settable
per step. A network step waiting on a rate limit occupies a network
slot; it does not stop a render.

### 2.5 Failure is a state on the row, not a fence across the graph

Today a failed step blocks its dependents for the run. In a loop with
no run boundary the natural rule is the streaming one: a consumer reads
its sink at a *committed* version, and a producer that failed midway
has committed what it committed. The consumer runs against that and is
right for that state. The failure is on the producer's row — the retry
policy exhausts, the row reads `failed` with its `FailureKind` and
error, **Retry** sets its want — and its `problems` rows travel down
with the data as they do now. A consumer never reads a torn store,
because a step commits atomically or the next writer's open seals its
dirty rows into a rescue commit (etl README § One writer per file).

### 2.6 What a step sees

Unchanged, plus `DATALIB_READS`: a JSON map of sink → version this
invocation was started against, so a consumer that pins does so at the
version the supervisor recorded as *consumed*. `DATALIB_DAG_NOW` is
pinned **per invocation**, not per run; one clock per run was a rule
about a unit this design does not have. `DATALIB_DAG_RUN_ID` becomes
`DATALIB_INVOCATION_ID`.

### 2.7 One store, one writer, everything the UI shows

`dag_state.json` goes. The supervisor's memory *is* `system/runs.sqlite`,
which it alone writes:

| table | rows |
|---|---|
| `steps` | id, mode, wanted_at, class, fingerprint, `state` (idle · stale · waiting(sink/budget) · running · paused · failed), state_detail |
| `sinks` | path, version, updated_at_utc, by_invocation |
| `invocations` | id, step, started/finished, exit, failure_kind, error, pid, consumed (json), produced (json), `caused_by` |
| `log`, `metrics`, `metric_samples` | as today |

`caused_by` is the invocation or the intent that made this one stale —
so "your Sync of Gmail" is a query (`WITH RECURSIVE` over `caused_by`),
and the UI can show a *wave*: ingest done, render running, index
waiting on its sink. That is the one thing "a run" gave the user that
is worth keeping, and it is derivable rather than a unit of execution.

The UI reads `steps.state` and is done. `manage/status.rs`'s inference
— `reached_since`, `spoken_for`, the walk up `waiting_on` — is deleted,
not ported. `sync_jobs` goes too: a job was a want with a process
attached; the want is a column and the process is an invocation.

### 2.8 Where it runs, and the CLI

The supervisor is a library (`datalib_dag` grows into it; the name can
follow). **`datalib-http` hosts it**, in place of `worker.rs`; the
server's own per-root lock is the one-supervisor guarantee, and
`runner-lock` is retired. `datalib-dag <config>` keeps working as
**batch mode**: want every source, tick until nothing is stale, wanted
or running, exit 0 or 1 — which is what the fixture genrule and CI
need, and is the same loop with a termination condition. When a server
holds the root the CLI forwards to it (`POST /api/steps/…/want`, which
is how "a sync you start from a terminal shows up here too" stays
true); when none does, it embeds.

### 2.9 Two operators: a person at the screen, an agent at a shell

Both steer the same way and both watch the same store. Observing and
steering are symmetric between them on purpose, because the agent case
is not hypothetical — `agent_user.md` exists because agents already run
syncs and read the mirror — and because a person and an agent will
often be working the same root at once.

**Observe.** `system/runs.sqlite` is plain SQLite, so an agent needs no
datalib binary to read it: `sqlite3 system/runs.sqlite 'select id,
state, state_detail from steps'` is the whole of the Manage screen's
Status column; `invocations` joined to `log` is the per-step log the
UI shows on double-click; `caused_by` gives the wave. The GUI reads
the same tables through `table_changed` frames. Nothing the screen
shows is computed in the browser from something the shell cannot see.

**Steer.** Four verbs — `want`, `pause`, `resume`, `stop` — one per
button, exposed identically as `POST /api/steps/<id>/<verb>` and as
`datalib-dag <verb> <step-id>` (the CLI forwards to the server that
holds the root, and acts directly when none does). An intent row
records `by` — `ui`, `cli`, or a name an agent passes (`--by claude`)
— so each operator sees the other's hand on the wheel: a source paused
by an agent reads "paused by claude" on the screen, and an agent that
finds a source paused can read who did it before deciding to resume
it. The rule for an agent is the one a good colleague follows: don't
resume what a person paused without saying so; the `by` column is
what makes that possible.

**The batch verbs are the same verbs.** `datalib-dag <config>` is
`want` on every source plus "exit when quiet"; an agent that wants
one chain synced and then wants to know when it settled runs
`datalib-dag want work-gmail/ingest --wait`, which polls `steps.state`
for the wave and exits with the wave's outcome. No agent should ever
have to `sleep` and re-check, which is the AGENTS.md rule for tests
applied to operators.

## 3. Hazards

- **Two writers of one sink and incrementality.** Each writer keeps its
  own cursor under its own tree; the *sink's* version moves when either
  commits. A reader is unaffected. But two writers that upsert the same
  keys with different opinions will fight, and doltlite will faithfully
  record the fight. The rule for a shared sink is the one the etl
  README already states for a raw store: the primary key is the
  upstream id, so two fetchers of one upstream agree by construction.
  Two *different* upstreams into one sink need disjoint keys, and the
  doc has to say so where `writes` is introduced.
- **`auto` steps run whenever stale, so a tick can start work nobody
  asked for** — the #225 case: a render of somebody else's 3.4 GB that
  was pending from yesterday. The supervisor is honest about it (the
  row says `running`, `caused_by` says why) and a person can pause it,
  which they could not before. But the *default* for a fresh install
  should make the first Sync of one source cost only that source's
  chain, so `auto` derived steps of a source whose ingest has *never*
  been wanted stay idle. Say this as a rule and test it.
- **Cheap sink versions are load-bearing.** A tick that hashes a tree
  to learn a sink's version is a tick that costs seconds; `stale()` runs
  on every event. Doltlite sinks are free (head commit). Markdown trees
  and qmd's sqlite need a reported or sidecar version; the runner's
  fallback of hashing on the step's behalf becomes "unknown, treat as
  moved once, then trust the step's next report" — a fallback, so it
  logs when it fires.
- **The tick is one function with the whole graph in scope.** That is
  the point, and also where a bug in `stale()` starts every step at
  once. Budgets bound the blast radius; a test that a tick on an
  all-fresh graph starts nothing is the first test written.
- **Reload while running.** A step whose fingerprint changed while
  running finishes under the old argv (we cannot change a running
  process), is then stale by fingerprint and runs again. A step removed
  from the config is interrupted and its rows dropped. A sink that
  loses its last writer keeps its version and its readers.
- **The batch mode's termination condition** — nothing stale, wanted
  or running — must hold on a graph where a source's retry policy is
  still counting down, or CI hangs. Retrying is `running` for this
  purpose; exhausted is `failed`, which terminates.
- **Doltlite is not made worse, but it is leaned on harder.** Every
  concurrency claim here rests on the per-file writer lock and pinned
  readers. `doltlite_two_process_test` becomes the test the supervisor
  is measured against, and the AGENTS.md warning stands: expect a
  scheduling change to pass locally and fail on CI, and count the opens
  first.

## 4. How it compares to the join

| | join (`join_running_sync.md`) | supervisor |
|---|---|---|
| a source started mid-sync | joins the run | just starts |
| Stop | per run | per step |
| Pause a source | no | yes, sticky |
| two steps into one store | no | yes, one at a time |
| streaming | the existing special cases | the ordinary rule |
| "queued" on screen | inferred from jobs + state file + timestamps | a column the supervisor wrote |
| an agent steering it | `POST /api/sync/jobs`, then read three stores | the same four verbs the buttons use; one plain-SQLite store, with `by` |
| what it keeps | everything; adds ~7 slices | step protocol, graph, versions, retry, run store, etl locks |
| what it removes | `parent_job_id` | `Runner::run`, `dag_state.json`, `worker.rs`, `sync_jobs`, `status.rs`'s inference, `runner-lock` |
| size | ~1.5 weeks | ~4 weeks, most of it deleting |
| risk | scheduler recount bugs | the two-process class of test |

The join is a patch to a batch runner. This is the thing the UI was
already pretending the batch runner was.

## 5. Order of work, if this is the way

Each slice lands green and the app works after each.

1. **Sinks in the graph.** `writes`/`reads` in the config with the
   defaults above; `Graph` bipartite; the loader allows a shared sink.
   No scheduler change yet: the current runner treats a shared sink as
   a diagnostic-level warning and runs as now. Tests: today's configs
   load identically; a shared sink loads.
2. **Sink versions from the store.** A doltlite sink's version is its
   head commit, read by the framework; the step's report is checked
   against it in tests, then becomes optional for doltlite sinks.
3. **The supervisor library**, batch mode only: the tick of §2.3 with
   every source wanted, run until quiescent. It passes the scheduler's
   existing tests re-expressed against invocations (the semantics they
   pin — subset sync, not-selected history, unselected trees never
   hashed — are the `auto`-default rule of §3 and hold). `datalib-dag`
   switches to it; the fixture genrule is the proof.
4. **Resident mode in `datalib-http`**, replacing `worker.rs`: intent
   table, `/api/steps/<id>/{want,pause,resume,stop}`, live frames.
   `sync_jobs` and `dag_state.json` go; `status.rs` shrinks to a read
   of `steps.state`.
5. **The UI**: per-row Sync/Stop/Pause, the wave view, the schedule
   field. The help text is rewritten around rows, not runs.
6. **Shared-sink providers**: split `grid_index` per source; the email
   import beside the pull. The reason §2.1 exists, landed last because
   everything before it is needed for it to be safe.
7. **Docs.** The dag README is rewritten around the tick;
   `step_protocol.md` gains `DATALIB_READS`; `streaming_steps*.md` move
   to `completed/` with a line saying the supervisor subsumed them;
   `join_running_sync.md` is deleted.
