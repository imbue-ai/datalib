# The supervisor: steps as managed processes, not as a batch run

**Status: greenfield proposal (2026-09-19, revised 2026-09-20), not built.** This is the
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
| **intent** | the open *requests* — each names roots and puts their downstream closure in scope — plus which steps are paused | the UI, and the CLI |
| **facts** | every sink's current version; every invocation and how it ended | the supervisor's store |

— and starts a step whenever it is *in scope of an open request*,
*stale*, *not paused*, *not running*, and *its sink is free*. There is
no run. There are requests, which say what someone asked for, and
invocations, which say what was done about it.

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
- **The run store is SQLite and already carries what a step is doing**
  (`system/runs.sqlite`: `step_runs`, `log`, `metrics`; pushed to the UI
  as `table_changed` frames by `watch.rs`). `logs_and_metrics` moved
  progress and logs there, but not the scheduler's memory: versions,
  fingerprints and `current_run` are still `system/dag_state.json`,
  rewritten by `scheduler.rs` on every dispatch and terminal state,
  watched by `watch.rs`, and served by `GET /api/dag`. Two records,
  half-migrated; this design finishes the move.
- **Streaming is built for the edges that matter.** `download → render`
  seals per provider boundary for claude, chatgpt, slack and email
  (Gmail and JMAP), and `render → grid_index` runs an index pass per
  seal, with a seal that lands mid-pass owed exactly one more
  (`streaming_steps_plan.md` slices 6–7). On the root this doc was
  measured on, `grid_index` passed at 22:06 and 22:10 while the Gmail
  ingest was still running. What is serialized is one index *pass* at a
  time, and a pass is a delta.
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
incremental one is the same wish for anything. The fan-ins are *not*
the case for it: `grid_index` already streams a pass per seal from any
render, so the only thing one shared index step serializes is the pass
itself, and a pass is small. Splitting it per source is possible under
this design and not worth doing first.

**A sink's version belongs to the sink.** For a doltlite store it is
the head commit, read by the supervisor after any writer finishes
(`datalib_history` already reads a store's log without linking `etl`).
For a tree with no store of its own — a markdown tree, qmd's
`index.sqlite` — the step reports it as it does today, and the last
report stands. The step's `outcome` line keeps working unchanged; it is
just no longer the *only* way a sink's version can move.

### 2.2 Intent: requests, and what they put in scope

Nothing runs because it is stale. A step runs because it is stale
**and someone asked for the chain it is on**. The asking is a
**request**: a durable row naming one or more root steps, and the
request's **scope** is those roots plus everything downstream of them —
the same reachability `runnable_subgraph` computes for `--sync` today,
made a first-class thing the supervisor holds many of at once.

| | |
|---|---|
| **Sync** on a source | a request with that source as its root |
| **Sync everything** | a request with every source as a root |
| a schedule (`every: 15m` on a source) | a request the supervisor creates when it comes due |
| `datalib-dag <config>` | a request with every source, and "exit when it closes" |
| **Stop** on a request | interrupts its running steps and closes it; a step also in another open request's scope stays in scope |
| **Pause** on a step | sticky; the step never starts and is interrupted if running, whatever requests want it. **Resume** lifts it |

A request is **open** until every step in its scope is fresh and not
running, then **done**; if a step in its scope exhausts its retries the
request is **failed** and says which step. A request that is closed is
history: nothing in the graph is in scope of it any more.

This is the answer to #225 by construction rather than by care: a
render that has been stale since yesterday sits with its row saying
`stale` until somebody's request reaches it, and nothing on the screen
can move a byte that nobody asked to move. It is also what today's
`--sync` means, kept — the difference is that today there can be one
of these at a time and it is planned once; here there are as many as
people have asked for, they overlap, and they are open to be looked
at, added to and stopped while they run.

**Staleness is request-relative at the roots.** A derived step is stale
by the ordinary rule (§2.3). A source has no `reads`, so today "always
runs"; here it is stale *for a request* iff no invocation of it has
completed that started after the request was created. Sync pressed on
a running source therefore opens a request whose root is running; when
that invocation ends it does not count (it started earlier), the root
is still stale for the new request, and it runs once more. One more
pass, no bookkeeping.

Requests and pauses are stored, not inferred: rows in the supervisor's
store, so a restart picks up the open requests where it left them, and
a paused source stays paused across app launches — which today has no
representation at all.

### 2.3 The reconcile tick

One loop, one function, run on every event (a step finished, a
checkpoint arrived, a request opened or was stopped, a step was paused,
the config changed, a schedule came due) and on a slow timer as the
fallback:

```
scope = union of closure(r.roots) for r in open requests
for step in graph, in topological order:
    if running(step):                       continue
    if paused(step):                        state = paused;          continue
    if step not in scope:                   state = idle | stale;    continue
    if not stale(step, requests wanting it): state = fresh;          continue
    if sink_busy(step.writes):              state = waiting(sink);   continue
    if budget_exhausted(step.class):        state = waiting(budget); continue
    start(step, consumed = versions of step.reads right now)
close every open request whose scope is all fresh and none running
```

`stale(step)` for a derived step is today's predicate, unchanged in
substance: never succeeded, or some read sink's version differs from
what its last successful invocation consumed, or its own fingerprint
changed. For a root it is the request-relative rule of §2.2.

Everything the runner's loop needed special machinery for falls out:

- **A consumer runs while its producer is still running.** A checkpoint
  moves the sink's version; the consumer is stale and in scope; the tick
  starts it against the checkpointed commit. When the producer finishes,
  the sink moves once more; if the consumer consumed an earlier version
  it is stale again and runs once more, else it is fresh. No
  `final_pass_owed`, because there is no final pass — only "is it stale
  now, and does someone want it".
- **A source added mid-sync starts now.** The config reload adds a
  step; Sync opens a request rooted at it; the tick starts it. Nothing
  to join.
- **Two requests share a fan-in.** Gmail's request and Slack's both
  reach `grid_index`; it is in scope while either is open, runs when
  stale, and Stop on one request leaves it in the other's scope.
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
no run boundary the rule is the streaming one: a consumer reads its
sink at a *committed* version, and whatever is committed is right for
that version. A producer that failed midway has committed what it
committed and the consumer indexes that; a producer that failed before
committing anything has not moved its sink, and the consumer is fresh
and does nothing. The failure is on the producer's row — the retry
policy exhausts, the row reads `failed` with its `FailureKind` and
error, **Retry** opens a request rooted at it — and its `problems`
rows travel down with the data as they do now.

That rests on one rule, and it is stricter than the one
`step_protocol.md` states today:

> **A commit is a correct state. Never leave a torn tree on *any*
> path** — success, failure, interrupt, or crash.

The protocol today asks for atomicity on the success path and
recoverability on the others, because the only reader was the next run
of the same step. Here every commit has readers at once. Two places
the tree has to change to honour it:

- **The rescue commit goes.** A writer's `open` that finds a crashed
  predecessor's dirty rows *seals them into a rescue commit*
  (`doltlite_raw.rs::rescue_dirty_working_tree`, etl README § One
  writer per file). That is a torn state committed for everyone to
  read, and its own fallback says the rest: "the next ETL commit will
  fold the dirty rows in implicitly", because every commit is `-Am`.
  So not just the rescue but the sweep behind it: a writer's `open`
  **discards** the working set (`dolt_reset --hard`, or doltlite's
  equivalent) before it does anything else. With seals, what a crash
  loses is the delta since the last checkpoint, and refetching it from
  the cursor is what idempotency promises. The interrupt path has the
  same rule: a SIGINT commits only at a boundary the provider chose
  (the `Checkpointer` seal); a hook that commits whatever is in flight
  is a rescue by another name and goes with it. This one does not wait
  for the supervisor — it is a change to `RawDb::open` and lands on
  its own (§5, slice 0).

- **Truncation is never an implementation detail of an incremental
  step.** The truncate-before-refill shape is the one the streaming
  plan fenced with `Policy::Never`, because a store mid-wipe is a gap.
  Under this rule an incremental step that empties a table on its way
  to refilling it may not commit in between: the wipe and the refill
  are one commit, or the step uses the deletion shape the four
  streaming providers already have — prune to an enumeration walked to
  completion, so between commits the store is a superset, never a gap.
  `always_clear_before_ingest` is this case, not a user wipe: it is
  how a source fed by a complete snapshot gets its deletions
  ("the snapshot is the enumeration", `data_architecture_ingestion.md`),
  and it becomes wipe-and-refill in one commit, invisible between:
  **don't commit until it's done.** That forgoes checkpoints and makes
  a crash lose the pass, which costs nothing for the inputs that use
  it — a Lightroom catalog, a Takeout export, a phone backup, a
  directory of `.vcf` files — all local and fast to read.
  A wipe a *person* asks for is a different thing and gets its own
  operation (§2.10); `--reset-and-redownload` retires in its favour.

`step_protocol.md`'s rules paragraph is rewritten to say this, and the
lint that watches render reads for a pin gains a sibling that watches
for a commit between a truncate and its refill in any step.

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
| `requests` | id, roots, `by`, created_at_utc, closed_at_utc, `state` (open · done · failed · stopped), failed_step |
| `steps` | id, paused_by, schedule, class, fingerprint, `state` (idle · stale · fresh · waiting(sink/budget) · running · paused · failed), state_detail |
| `sinks` | path, version, updated_at_utc, by_invocation |
| `invocations` | id, step, started/finished, exit, failure_kind, error, pid, consumed (json), produced (json) |
| `request_steps` | request → step, for every step in the request's scope, with the step's state as of the request's close |
| `log`, `metrics`, `metric_samples` | as today |

A request *is* the wave — "your Sync of Gmail: ingest done, render
running, index waiting on its sink" is `request_steps` joined to
`steps` — so the one thing "a run" gave the user that was worth keeping
is a row, not a unit of execution and not a recursive query.

The UI reads `steps.state` and is done. `manage/status.rs`'s inference
— `reached_since`, `spoken_for`, the walk up `waiting_on` — is deleted,
not ported. `sync_jobs` goes too: a job was a request with one
process attached; the request is a row and the process is an
invocation.

### 2.8 Where it runs, and the CLI

The supervisor is a library (`datalib_dag` grows into it; the name can
follow). **`datalib-http` hosts it**, in place of `worker.rs`; the
server's own per-root lock is the one-supervisor guarantee, and
`runner-lock` is retired. `datalib-dag <config>` keeps working as
**batch mode**: open one request rooted at every source, tick until it
closes, exit 0 or 1 — which is what the fixture genrule and CI need,
and is the same loop with a termination condition. When a server holds
the root the CLI forwards to it (`POST /api/requests`, which is how "a
sync you start from a terminal shows up here too" stays true); when
none does, it embeds.

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
UI shows on double-click; `requests` joined to `request_steps` is the
wave. The GUI reads
the same tables through `table_changed` frames. Nothing the screen
shows is computed in the browser from something the shell cannot see.

**Steer.** Five verbs, one per button: `request <roots…>` and
`stop <request>` on requests, `pause <step>` and `resume <step>` on
steps, `clear <sink>` on sinks (§2.10) — exposed identically as
`POST /api/requests` and `/api/requests/<id>/stop`,
`POST /api/steps/<id>/{pause,resume}`, `POST /api/sinks/<path>/clear`, and
as `datalib-dag <verb> …` (the CLI forwards to the server that holds
the root, and acts directly when none does). Every request, pause and
clear records `by` — `ui`, `cli`, or a name an agent passes
(`--by claude`) — so each operator sees the other's hand on the wheel:
a source paused by an agent reads "paused by claude" on the screen,
and an agent that finds a source paused can read who did it before
deciding to resume it. The rule for an agent is the one a good colleague follows: don't
resume what a person paused without saying so; the `by` column is
what makes that possible.

**The batch verbs are the same verbs.** `datalib-dag <config>` is one
request rooted at every source plus "exit when it closes"; an agent
that wants one chain synced and to know when it settled runs
`datalib-dag request work-gmail/ingest --wait`, which polls the
request's row and exits with its outcome. No agent should ever
have to `sleep` and re-check, which is the AGENTS.md rule for tests
applied to operators.

### 2.10 Clear is its own operation

Emptying a sink is something a person asks for, on purpose, and it
deserves its own verb and its own button rather than a flag on a
download. **`clear <sink>`** is a framework step, not a provider's: it
takes the sink's writer lock like any writer, empties every table in
the store — entities, sidecars, cursors, because the etl README keeps a
store's bookkeeping *in* the store — and commits once. In the doltlite
sense nothing is gone: the commit before it is still there, the
history panel shows it, and a wrong click is a revert, which is why
this can be a button at all. The UI says exactly that: "Clear Work
Gmail — every row goes, the history keeps them, the next Sync
re-downloads from nothing."

A clear opens a request rooted at the sink (a sink can be a root: its
scope is its readers' closure), so the emptiness propagates the way any
change does — the render's diff sees every bucket deleted and removes
its documents, the index drops the rows — and stops there. Refilling
is not part of it; that is the next Sync, which finds no cursor and
starts from the beginning. `--reset-and-redownload` becomes
`clear` followed by `request`, and the ingest code loses its
`reset_and_redownload` branch: a download never wipes, it only
downloads.

Two sinks need a word. A render store is doltlite and clears the same
way. The qmd index is a plain SQLite file with no history; clearing it
is deleting it, and the UI's wording for that sink is different
because the promise is different.

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
- **Scope is the only thing between a tick and #225.** Nothing runs
  outside an open request's closure, so a stale render from yesterday
  cannot start on its own — but a request rooted at *every* source (Sync
  everything, the batch CLI, a schedule on a wide source) does reach
  it, honestly, and the row says which request. The test to write
  first: a tick over a graph with stale steps and no open request
  starts nothing.
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
  process), is then stale by fingerprint and runs again if a request
  still has it in scope. A step removed
  from the config is interrupted and its rows dropped. A sink that
  loses its last writer keeps its version and its readers.
- **The batch mode's termination condition** — its one request closes
  — must hold on a graph where a source's retry policy is
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
| Pause a source | no | yes, sticky, across restarts |
| several syncs open at once, each stoppable | no | yes: requests |
| two steps into one store | no | yes, one at a time |
| clear a store | a flag on the download | its own verb, its own button, reverted from the history |
| streaming | the existing special cases | the ordinary rule |
| "queued" on screen | inferred from jobs + state file + timestamps | a column the supervisor wrote |
| an agent steering it | `POST /api/sync/jobs`, then read three stores | the same four verbs the buttons use; one plain-SQLite store, with `by` |
| what it keeps | everything; adds ~7 slices | step protocol, graph, versions, retry, run store, etl locks |
| what it removes | `parent_job_id` | `Runner::run`, `dag_state.json` (the half `logs_and_metrics` left), `worker.rs`, `sync_jobs`, `status.rs`'s inference, `runner-lock` |
| size | ~1.5 weeks | ~4 weeks, most of it deleting |
| risk | scheduler recount bugs | the two-process class of test |

The join is a patch to a batch runner. This is the thing the UI was
already pretending the batch runner was.

## 5. Order of work, if this is the way

Each slice lands green and the app works after each.

0. **The rescue commit goes** (§2.5). `RawDb::open` discards a dirty
   working set instead of sealing it; the interrupt hooks are audited
   for a commit outside a seal boundary. `doltlite_raw.rs`'s "phase 2"
   test, which today asserts the rescue swept the orphaned writes,
   asserts they are gone and the store is at its last commit. Lands
   before anything else and under either plan.
1. **Sinks in the graph.** `writes`/`reads` in the config with the
   defaults above; `Graph` bipartite; the loader allows a shared sink.
   No scheduler change yet: the current runner treats a shared sink as
   a diagnostic-level warning and runs as now. Tests: today's configs
   load identically; a shared sink loads.
2. **Sink versions from the store.** A doltlite sink's version is its
   head commit, read by the framework; the step's report is checked
   against it in tests, then becomes optional for doltlite sinks.
3. **The supervisor library**, batch mode only: the tick of §2.3 with
   one request rooted at every source, run until it closes. It passes
   the scheduler's existing tests re-expressed against requests and
   invocations (the semantics they pin — subset sync, not-selected
   history, unselected trees never hashed — are exactly what scope
   means, and hold). `datalib-dag`
   switches to it; the fixture genrule is the proof.
4. **Resident mode in `datalib-http`**, replacing `worker.rs`: the
   `requests` and `request_steps` tables, `POST /api/requests`,
   `/api/requests/<id>/stop`, `/api/steps/<id>/{pause,resume}`, live
   frames.
   `sync_jobs` and `dag_state.json` go; `status.rs` shrinks to a read
   of `steps.state`.
5. **The UI**: per-row Sync and Pause, a requests panel with Stop per
   request and the wave under each, the schedule field, and Clear on
   a sink with the wording of §2.10. `--reset-and-redownload` and the
   ingest's wipe branch go in the same slice. The help text is rewritten around rows, not runs.
6. **A shared-sink provider**: the email import beside the live pull.
   The reason §2.1 exists, landed last because everything before it is
   needed for it to be safe.
7. **Docs.** The dag README is rewritten around the tick;
   `step_protocol.md` gains `DATALIB_READS`; `streaming_steps*.md` move
   to `completed/` with a line saying the supervisor subsumed them;
   `join_running_sync.md` is deleted.
