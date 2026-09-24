# The supervisor: steps as managed processes, not as a batch run

**Status: chosen over the join (2026-09-23); slices 0–3 and 4a are
built — `datalib-dag` runs the loop over requests in
`system/supervisor.sqlite`, or hands its request to the one already
running — and the rest is not.** This is the alternative to
[`join_running_sync.md`](join_running_sync.md), which patches the runner
we have. Both start from the same measurement (§0 there). This one asks
what we would build if the UI's needs came first. §1 describes the tree
as it stands at `b216a993`, after #600 and #606 (a writer's `open`
discards the working set; a stop ends a download at its next consistent
point), #682 and #686 (a process group per step), #687 (the run store
takes several writers) and #690 (writers work on a branch and publish to
`main`); nothing else here describes the tree. Where this doc and the
tree disagree, the tree wins.

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
  subprocess that writes under its tree, is idempotent, commits only
  correct states, reports content versions on stdout and checkpoints as
  it goes, and takes SIGINT as "stop at your next consistent point,
  commit there, exit 130" ([`step_protocol.md`](../step_protocol.md);
  the built-in ingests do this through `datalib_etl::stop::StopFlag`,
  #606). Nothing in this design changes what a step sees, except one
  environment variable (§2.6).
- **Nothing ever adopts a crashed writer's leftovers.** A doltlite
  writer's `open` discards a dirty working set — `dolt_reset --hard`
  plus the untracked tables it leaves — instead of sealing it into a
  "rescue" commit, and nothing commits from a signal handler (#600).
  Every commit in a store is therefore one a writer vouched for at a
  boundary it chose, which is what lets a consumer read at any commit.
- **One writer at a time per file is enforced where it matters.**
  `RawDb::open` takes `flock(2)` on `<store>.doltlite_db.lock` and
  refuses a second writer with the holder named; the kernel releases it
  on death. The etl README says outright that this is per file, not per
  root, "so two sources with nothing in common can be written by two
  runners at once (#247)". The storage layer permits exactly the
  concurrency the scheduler forbids. #691 measured the one alternative
  worth asking about — two writers on one file, each on a branch of its
  own — and most of their operations fail with `database is locked` or
  `commit conflict`. One writer at a time stays the rule, for a shared
  sink (§2.1) as much as for anything else.
- **A reader on `main` sees only sealed states** (#690). Every writer
  works on the `datalib_writer` branch and fast-forwards `main` when it
  seals (`publish_to_main`, inside `commit_run`). A reader pins a commit
  on `main`, so a writer's uncommitted batch — and a table it has
  created but not sealed — is invisible to it. A crash between the
  commit and the fast-forward leaves the branch ahead of `main`, and the
  *next writer's `open`* finishes the publish.
- **A step is a process group, and a stop reaches all of it** (#682,
  #686). `subprocess.rs` spawns each step as its group's leader through
  `process-wrap`, so the group id is the step's pid and a signal to the
  group reaches `node qmd embed` under `qmd_index` as well as the step;
  the wrapper reaps the group when the step exits. The worker's cancel
  is a three-rung ladder (`next_stage`: ask, tell the runner to give up
  on its steps, kill), and the runner forwards SIGHUP and its parent's
  death to the steps itself, because a group of its own no longer gets
  the kernel's. What is still shaped for one batch run: the pids live in
  one process-wide set (`CHILD_PIDS`), and the only operations on it
  are "interrupt all" and "kill all".
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
  (`system/runs/runs.sqlite`: `step_runs`, `log`, `metrics`; pushed to the UI
  as `table_changed` frames by `watch.rs`). It takes several writing
  processes at once without losing a line (#687, with
  `runs_two_process_test` as the measurement), so the batch CLI can
  write it while a server does. `logs_and_metrics` moved
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
never starts a second writer on a sink while one runs, and the sink's
own writer lock (the table below) is the backstop if something slips
past.

Why want it: the `claude` provider's api and export methods share one
store today by being one step with two methods; an mbox import beside
a live Gmail pull is the same wish for email; a backfill step beside an
incremental one is the same wish for anything. The fan-ins are *not*
the case for it: `grid_index` already streams a pass per seal from any
render, so the only thing one shared index step serializes is the pass
itself, and a pass is small. Splitting it per source is possible under
this design and not worth doing first.

#### What a sink has to be

Everything above is stated for doltlite because that is what the raw
and render stores are, but the supervisor never opens a store; it
schedules against three properties, and any storage that has them is a
sink. A new kind of sink is a new implementation of these, not a change
to the tick.

| property | what the supervisor needs | why |
|---|---|---|
| **a version** | a string that is a function of the sink's *committed* content, cheap to read without opening the sink for writing | `stale()` compares it on every tick; equal strings mean nothing moved |
| **atomic publish** | a reader sees only states a writer committed whole; a writer that dies leaves nothing a reader can reach | a consumer runs against whatever is published, at any moment, with no run boundary to hide behind |
| **one writer at a time** | the supervisor never starts a second writer while one runs, and the sink refuses one if something slips past | the same rule the etl README enforces per file today |

How the sinks we have meet them:

| sink | version | atomic publish | writer lock | on a crash |
|---|---|---|---|---|
| a doltlite store (raw, render) | `main`'s head — the last *published* seal, not the writer branch's | `commit_run` at a seal: commit on `datalib_writer`, fast-forward `main`; readers pin a commit on `main` | `flock` on the sibling `.lock` | the next `open` discards the working set, and publishes a commit the crash left unpublished |
| a plain SQLite file (qmd's `index.sqlite`, `runs.sqlite`) | a version the writer records *in the same transaction* as the data — a row in a `versions` table, or `PRAGMA user_version` | the transaction; readers open read-only | SQLite's own writer lock, plus the supervisor's scheduling | the transaction rolls back by itself |
| a file tree indexed by a store (the markdown documents) | the version of the store that indexes it: a `.md` file is reachable only through a committed `markdowns` row, so the tree has no version of its own | write the file, then commit the row that names it; a file no committed row names is unreachable | the indexing store's | orphan files are junk the next writer may remove; readers never saw them |
| a plain file tree (perseus's TEI XML, written by `curl -o`) | a content hash of the tree, computed by the supervisor **once, when its writer finishes** — the runner's hash today — and held until the next writer finishes | none of its own: a reader listing the tree mid-write sees a half-written file, so the supervisor starts no writer while a reader of the sink runs (or the step writes beside and renames) | the supervisor's scheduling alone | the next completed write re-hashes the whole tree, half-written files included, so nothing is adopted silently |
| a step that declares its own version | whatever its `outcome` line says, held by the supervisor until the next report | the step's promise, under the protocol's any-path rule | the supervisor's scheduling alone | the step's promise |

The last two rows are how every non-store sink works today and stay
available; the first three are what the supervisor can read for
itself. What matters is *when* a version is computed, not how: the
tree hash is a fine version for fourteen XML files, and a poor one for
a multi-gigabyte index — so a sink whose version is expensive to
compute is a sink that should record one instead (the qmd index, a
SQLite file, gets a `versions` row). Either way the supervisor
computes or reads a version **once per completed invocation** and the
tick compares cached strings; nothing is ever hashed inside the tick.

For a doltlite sink the version is `main`'s head, read by the
supervisor after **every** writer invocation ends — success, failure or
interrupt — because a writer's `open` can publish its crashed
predecessor's last commit, so a sink can move at the *start* of an
invocation that then fails (`datalib_history` already reads a store's
log without linking `etl`). The step's `outcome` line keeps working
unchanged; it is just no longer the *only* way a sink's version can
move.

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
| `datalib-dag <config>` | a request with every source, and "exit when it closes" |
| **Stop** on a request | interrupts its running steps and closes it; a step also in another open request's scope stays in scope |
| **Pause** on a step | sticky; the step never starts and is interrupted if running, whatever requests want it. **Resume** lifts it |

A request is **open** until nothing in its scope is running or due to
run, then **done** — or **failed**, naming the step, if a step in its
scope exhausted its retries (§2.3 has the exact rules). A request that
is closed is history: nothing in the graph is in scope of it any more.

Root staleness below compares *when* things happened: an invocation
that started after a request opened. The supervisor orders those
events with a sequence number it hands out itself, not with wall-clock
stamps, so the comparison is exact and a test can write it down.

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

**Scheduled syncs are out of scope.** A schedule would be one more
thing that opens requests, so it can be added later without touching
the tick; nothing here depends on it.

Requests and pauses are stored, not inferred: rows in the supervisor's
store, so a restart picks up the open requests where it left them, and
a paused source stays paused across app launches — which today has no
representation at all.

### 2.3 The reconcile tick

One loop, one function, run on every event (a step finished, a
checkpoint arrived, a request opened or was stopped, a step was paused,
the config changed) and on a slow timer as the
fallback:

```
scope = union of closure(r.roots) for r in open requests
for step in graph, in topological order:
    if running(step):                        state = running
                                             stop it if paused or out of scope
                                             continue
    if paused(step):                         state = paused;            continue
    if step not in scope:                    state = idle | stale | failed; continue
    if failed for every request wanting it:  state = failed;            continue
    if not stale(step, requests wanting it): state = fresh;             continue
    if a producer it reads is pending:       state = waiting(upstream); continue
    if sink_busy(step.writes):               state = waiting(sink);     continue
    if budget_exhausted(step.class):         state = waiting(budget);   continue
    start(step, consumed = versions of step.reads right now)
close every open request with nothing in its scope running or due
```

`stale(step)` for a derived step is today's predicate, unchanged in
substance: never succeeded, or some read sink's version differs from
what its last successful invocation consumed, or its own fingerprint
changed. For a root it is the request-relative rule of §2.2.

Three rules the loop needs that the runner got for free from having a
run:

- **A consumer waits for a producer for two reasons, and only two.**
  The producer is running and does not declare `streams_output`, so its
  sink may be half-written; or it is about to run, held only by a
  budget or by its sink, and will rewrite what the consumer would read.
  A running producer that streams never holds its consumers back: they
  start on each seal as it lands, and staleness keeps them from running
  when nothing new has. A producer that is itself waiting on something
  upstream does not hold them back either — it may not run for a long
  time, and a fan-in that waited on it would wait for its slowest
  source. (Two drafts got this wrong, each caught by a test: one asked
  a streaming producer to have published since it started, the other
  counted a producer waiting on its own upstream; both made the index
  wait for the slowest download.) A producer that failed or is paused
  holds nothing back, and its consumers run against what it committed
  (§2.5).
- **Nothing to read is `blocked`.** A stale step none of whose inputs
  has ever been published — a render whose first download failed — has
  nothing to read. It does not start, does not hold its request open,
  and fails the request. A fan-in reads whichever of its inputs exist,
  so one never-synced source does not block the index.
- **A failure is not retried by the tick.** Retries happen inside an
  invocation, as today (`invoke_with_retry`), so a retrying step reads
  `running`. Once they are exhausted the step is **failed for** every
  request opened before that invocation started, and the tick does not
  start it again for them — until what it reads moves (a producer
  sealed more; a new version is a new question) or its definition
  changes. A new request, **Retry** included, is a new question too.
- **A paused step does not hold a request open.** A request closes when
  nothing in its scope is running or due; a paused step is neither, and
  the request's `request_steps` row records it as paused. Otherwise one
  paused source would keep "Sync everything" open until someone resumed
  it, and the batch CLI would never exit.

A request's outcome when it closes: **failed**, naming the first step
in topological order that failed for it, if any did; **done** otherwise.
A failure does not end a request early. The rest of its scope runs to
completion, the way a failed source today does not stop the others.

**The tick is the functional core** in the sense of
[`style.md`](../style.md): it takes values — the graph, the open
requests and pauses, every sink's version, which steps are running —
and returns values — each row's state and the starts to make, with
the versions they consume — and touches nothing else. The shell
around it turns an event (a step exited, a checkpoint line arrived, a
request row was inserted, the config changed) into an update to those
values, calls the tick, and does what it said: spawns, writes the
`steps` and `invocations` rows, pushes the frame.

That is what makes every scheduling claim in this document a
synchronous test — no tokio, no tempdir, no polling: "a tick with
stale steps and no open request starts nothing", "a checkpoint
mid-pass owes exactly one more pass", "Stop on one request leaves the
fan-in in the other's scope", each a few values in and a list of
starts out. `scheduler.rs` keeps the same facts as parallel
`Vec<bool>`s mutated between `JoinSet::spawn` and `state.save`, and
its streaming tests have to run real tasks and wait on a flag to see
them; its `Decision::{Run, Skip, Block}` is already the shape of the
tick's output.

Everything the runner's loop needed special machinery for falls out:

- **A consumer runs while its producer is still running.** A checkpoint
  moves the sink's version; the consumer is stale and in scope; the tick
  starts it against that version. When the producer finishes,
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

The protocol used to ask for atomicity on the success path and
recoverability on the others, because the only reader was the next run
of the same step. Here every commit has readers at once. The tree
honours the stricter rule since #600 and #606, and
[`step_protocol.md`](../step_protocol.md) states it:

- **Nothing adopts a crashed writer's leftovers.** A doltlite writer's
  `open` used to seal a predecessor's dirty rows into a "rescue"
  commit — a torn state committed for everyone to read, and because
  every commit is `-Am`, the same rows rode into the next commit even
  when the rescue failed. `open` now discards the working set
  (`dolt_reset --hard`, then the untracked tables reset leaves behind)
  before doing anything else, and a schema commit provably carries no
  rows. For the other sink kinds the same rule is met the way §2.1's
  table says: a SQLite transaction rolls back on its own, an unnamed
  file in a tree is unreachable. With seals, a crash loses the delta
  since the last one, and the cursor refetches it.
- **Nothing commits from a signal handler.** SIGINT raises a stop flag;
  the download ends at its next consistent point, `finish` commits
  there, and the step reports `cancelled`. A stopped run does not record
  its scope config as satisfied, so a widened filter interrupted
  part-way is backfilled by the next run
  ([`data_architecture_ingestion.md` § A claim of completeness…](../data_architecture_ingestion.md)).

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
  operation (§2.10); `datalib-dag --reset` is that operation's CLI form today.

`step_protocol.md`'s rules paragraph is rewritten to say this, and the
lint that watches render reads for a pin gains a sibling that watches
for a commit between a truncate and its refill in any step.

### 2.6 What a step sees

Unchanged, plus `DATALIB_READS` (built): a JSON map of sink → version this
invocation was started against, so a consumer that pins does so at the
version the supervisor recorded as *consumed*. `DATALIB_DAG_NOW` is
pinned **per invocation**, not per run; one clock per run was a rule
about a unit this design does not have. `DATALIB_DAG_RUN_ID` becomes
`DATALIB_INVOCATION_ID`.

### 2.7 One store, one writer, everything the UI shows

`dag_state.json` goes. The supervisor's memory *is*
`system/supervisor.sqlite` — plain SQLite, beside the run store rather
than in it. The run store is deleted and recreated whenever its schema
moves (`SCHEMA_VERSION`), which would forget open requests and pauses;
this store's schema only ever grows, because two builds — an agent's CLI
and an older app — may share it. It is the **mailbox as well as the
record** (§2.8): anyone may write intent into it, and only the process
running the loop writes facts.

| table | rows |
|---|---|
| `requests` | id, roots, `by`, created_at_utc, closed_at_utc, `state` (open · done · failed · stopped), failed_step |
| `steps` | id, paused_by, class, fingerprint, `state` (idle · stale · fresh · waiting(sink/budget) · running · paused · failed), state_detail |
| `sinks` | path, version, updated_at_utc, by_invocation |
| `invocations` | id, step, started/finished, exit, failure_kind, error, pid, consumed (json), produced (json) |
| `request_steps` | request → step, for every step in the request's scope, with the step's state as of the request's close |
| `log`, `metrics`, `metric_samples` | as today, in the run store |

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

The supervisor is **one library** (`datalib_dag` grows into it; the
name can follow), and both ways of running it run that library, the
same loop, unmodified:

| host | what it adds around the loop |
|---|---|
| `datalib-http` | runs the loop for as long as it is up, in place of `worker.rs` |
| `datalib-dag` | runs the loop only when nobody else is, and only until every open request has closed |

There are no modes, and there is no forwarding. **Intent is rows**: a
request, a stop, a pause, a resume, a clear is a row in
`system/supervisor.sqlite`, written by whoever wants it — the UI through
the server, an agent through the CLI, a person with bare `sqlite3` — and
tagged with `by`. **Exactly one process runs the loop**: the one holding
`system/runner-lock`. It notices new intent by polling the store's
`PRAGMA data_version` (a read of one integer, sub-second), acts on it,
and writes the facts: steps' states, invocations, sinks' versions,
requests closing. Nothing else writes facts.

Who holds the lock:

- **The server, while it is up.** Whoever runs the loop owns the steps'
  processes, and a running process cannot be handed to another. A loop
  in an agent's CLI would carry the UI's syncs inside the agent's
  process: its `--wait` could not return before your forty-minute Gmail
  sync finished, and killing its shell would kill that sync. A process
  that outlives every caller is the right owner, and while the server is
  up it is that process.
- **The CLI, when nobody does.** It serves every open request —
  including ones the UI or a second CLI add meanwhile — and exits when
  none is left. A server that starts meanwhile waits for the lock; the
  UI's clicks are rows, so the CLI's loop runs them in the meantime.
- **A CLI that finds the lock taken is a client.** It writes its
  request, follows it — progress from the run store, Ctrl-C marks *its*
  request stopped — and exits 0 or 1 with the request's outcome.

So an agent drives the system while the UI is up, and the UI while an
agent's round runs: neither waits on the other, and each sees the
other's requests live because both read the same tables. A request
outlives the process that was running it: if that process dies, its
steps die with it (below), but the request is still a row, and the next
process to take the lock runs it.

**The host owns the steps' processes**, and the library is where that
lives, so no host does it differently:

- **A handle per invocation, not a set of pids.** Stop on one row
  signals that step's process group (#686) and no other. `CHILD_PIDS`
  and its "interrupt all / kill all" become the library's shutdown
  path, not its only way to reach a step.
- **The cancel ladder is per invocation.** Today's three rungs
  (`worker.rs::next_stage`) are aimed at the whole `datalib-dag`
  process; they move down to one step's group: SIGINT, SIGINT again
  after the grace, SIGKILL after that. `next_stage` stays the pure
  function it is.
- **The host's own death takes its steps with it.** The batch runner
  already forwards SIGHUP and its parent's death (#682); the server
  needs the same, because the runner that used to sit between it and
  the steps is gone. A step that
  outlives its supervisor is the orphan #682 fixed, with nobody to
  record its end.
- **An invocation left open by a dead supervisor is closed by the next
  one** at startup: its row is `running`, its process group is gone,
  so it is marked `stopped`. This is the cancel plan's "a cancelled run
  closes its own books" (PR 2 of
  [`cancel_and_log_hygiene.md`](cancel_and_log_hygiene.md)), absorbed
  rather than built twice.

### 2.9 Two operators: a person at the screen, an agent at a shell

Both steer the same way and both watch the same store. Observing and
steering are symmetric between them on purpose, because the agent case
is not hypothetical — `agent_user.md` exists because agents already run
syncs and read the mirror — and because a person and an agent will
often be working the same root at once.

**Observe.** Both stores are plain SQLite, so an agent needs no
datalib binary to read them: `sqlite3 system/supervisor.sqlite 'select
id, state, state_detail from steps'` is the whole of the Manage screen's
Status column; `requests` joined to `request_steps` is the wave;
`invocations`, and the run store's `log`, are the per-step log the UI
shows on double-click. The GUI reads
the same tables through `table_changed` frames. Nothing the screen
shows is computed in the browser from something the shell cannot see.

**Steer.** Five verbs, one per button: `request <roots…>` and
`stop <request>` on requests, `pause <step>` and `resume <step>` on
steps, `clear <sink>` on sinks (§2.10) — exposed identically as
`POST /api/requests` and `/api/requests/<id>/stop`,
`POST /api/steps/<id>/{pause,resume}`, `POST /api/sinks/<path>/clear`, and
as `datalib-dag <verb> …`. Both doors write the same row (§2.8): the
HTTP API is the UI's, since a browser cannot write SQLite, and the CLI
is the agent's — it works whether or not a server is up, needs no port
or token, and checks a step id against the config before it writes
anything. The token is not bypassed in any way that matters: whatever
can write the data root can read `system/api-token`. Every request, pause and
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
`datalib-dag request work-gmail/ingest --wait`, which writes the
request's row, follows it, and exits with its outcome — whether its own
process runs the loop or the server's does. No agent should ever
have to `sleep` and re-check, which is the AGENTS.md rule for tests
applied to operators.

### 2.10 Clear is its own operation

Emptying a sink is something a person asks for, on purpose, and it
deserves its own verb and its own button rather than a flag on a
download. **`clear <sink>`** is a framework step, not a provider's: it
takes the sink's writer lock like any writer, empties the sink — for a
store, every table: entities, sidecars, cursors, because the etl README
keeps a store's bookkeeping *in* the store — and publishes that once,
as one version. For a doltlite sink nothing is gone: the commit before
it is still there, the history panel shows it, and a wrong click is a
revert, which is why this can be a button at all. The UI says exactly
that: "Clear Work Gmail — every row goes, the history keeps them, the
next Sync re-downloads from nothing."

A clear opens a request rooted at the sink (a sink can be a root: its
scope is its readers' closure), so the emptiness propagates the way any
change does — the render's diff sees every bucket deleted and removes
its documents, the index drops the rows — and stops there. Refilling
is not part of it; that is the next Sync, which finds no cursor and
starts from the beginning. `datalib-dag --reset` (built,
`doltlite_raw::reset_store`) is `clear`, and
`--reset X --sync X` is `clear` followed by `request`; the ingest code
has no reset branch: a download never wipes, it only downloads.

What "clear" promises depends on the sink kind (§2.1's table), and the
supervisor knows which it is talking to. A doltlite sink — raw or
render — clears to an empty commit with the history intact: the button
can say "the history keeps them". A sink without history — qmd's
`index.sqlite`, a plain tree — is emptied for good, and the button says
that instead. A sink that only a step's report versions is cleared by
the step, through the same `clear` verb passed to it, because the
supervisor cannot know its shape.

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
  everything, the batch CLI) does reach
  it, honestly, and the row says which request. The test to write
  first: a tick over a graph with stale steps and no open request
  starts nothing.
- **A version is computed when a writer finishes, never in the tick.**
  `stale()` runs on every event, so anything it does is done constantly;
  it compares strings the supervisor already holds. The tree hash a
  plain-tree sink needs (perseus) is computed once, when its writer
  completes, exactly as the runner does today. What makes that hash
  wrong is size, not principle: `qmd_index` reports no version and has
  its whole `index.sqlite` hashed after every pass, which is the one
  place the cost shows; slice 2 gives it a `versions` row. A new sink
  whose tree is large gets the same treatment before it gets a row in
  the graph.
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
- **A non-doltlite sink is only as safe as its row in §2.1's table.**
  The three properties are easy to claim and easy to half-meet: a
  SQLite version written in a *separate* transaction from the data is
  a version that can lie; a file tree whose files are reachable by
  path before their row is committed publishes torn state to anyone
  listing the directory. Each new sink kind gets the same two-process
  test doltlite has — a writer killed mid-write, a reader that must
  see the old version and nothing else — before the supervisor is
  allowed to schedule against it.

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
| an agent steering it | `POST /api/sync/jobs`, then read three stores | the same five verbs the buttons use; one plain-SQLite store, with `by` |
| what it keeps | everything; adds ~7 slices | step protocol, graph, versions, retry, run store, etl locks |
| what it removes | `parent_job_id` | `Runner::run`, `dag_state.json` (the half `logs_and_metrics` left), `worker.rs`, `sync_jobs`, `status.rs`'s inference, `runner-lock` |
| size | ~1.5 weeks | ~4 weeks, most of it deleting |
| risk | scheduler recount bugs | the two-process class of test |

The join is a patch to a batch runner. This is the thing the UI was
already pretending the batch runner was.

## 5. Order of work, if this is the way

Each slice lands green and the app works after each. The core comes
first; the shared sink, the feature that needs the most of it, comes
last.

0. ~~**The rescue commit goes**~~ **Done: #600, #606.** `RawDb::open`
   discards a dirty working set instead of sealing it, and the
   interrupt commit hook is gone; SIGINT raises a stop flag and the
   download ends at its next consistent point with a real final commit.
   Taking the rescue away exposed that `RawStoreSession::finish` never
   committed the blob CAS — the next run's rescue had been doing it —
   and that jmap saved its state token before its enumeration finished.
   Both fixed there.
1. **The tick** (`dag/src/supervisor/tick.rs`): §2.3 as a pure
   function over values, with nothing else in the change. Its tests are
   synchronous and come first; the two hazards in §3 that say "the
   first test written" are the first two. The graph it takes already
   names sinks (`writes`, `reads`), but only in the shape the tree has
   today: each step writes the sink its id names. Nothing calls it yet.
2. ~~**The batch host.**~~ **Built.** `supervisor/round.rs` is the
   body of `Runner::run`: one request rooted at every source (or the
   `--sync` roots), ticked until it closes, facts read from and written
   back to `dag_state.json`, and the events the run store and the server
   already read. Each invocation has its own stop handle
   (`StepCtx::stop`, SIGINT to its process group); the first SIGINT or
   SIGTERM to `datalib-dag` stops the round through it. The old loop and
   its `in_flight` / `final_pass_owed` / `streaming_pass_owed` are gone.
   The scheduler's own tests pass against it with four changed on
   purpose: a failed producer's committed output is read by its
   consumers, and a failed render no longer blocks the index (§2.5).
3. **Sink versions from the sink.** ~~A doltlite sink's version is
   `main`'s head, read by the host after every writer invocation
   (§2.1); the step's report is checked against it in tests, then
   becomes optional for doltlite sinks.~~ **Built:** `dag/src/sink.rs`
   reads `main`'s head of every store at the top of a step's tree, after
   every invocation and at every checkpoint, and a step's report is used
   only for a tree with no store. It fixed a waste nobody had seen: a
   step's checkpoints and its outcome spelled one commit differently
   (`<hash>` against `store:<hash>`), so every finishing ingest and
   render made its consumers run one more pass over nothing. **Also
   built:** `DATALIB_READS` (§2.6), and the qmd index reports a hash of
   it — the render versions it indexed — instead of having its tree
   hashed. The two unpinned readers are marked by the loader
   (`UNPINNED_BUILTINS`), and the tick keeps each apart from the writers
   of what it reads, in both orders: the reader waits even for a
   streaming producer, and a writer waits in `waiting(reader)`. The cost
   is some streaming: the qmd index no longer runs while a render does.
4. **The mailbox, then the server**, in three slices, each landing
   green. A map of what the server's sync path touches (2026-09-23)
   found the couplings that decide the order; each is named where it is
   dealt with.

   **4a. The store, and the CLI as loop or client.** *Built, narrower
   than first written:* the store holds `requests` and `pauses` only
   (steps' states and invocations come with 4c), the facts stay in
   `dag_state.json`, and the verbs (`stop`, `pause`, `resume` as
   commands) follow separately — the store takes them already. *Since
   built:* `datalib-dag status | stop | pause | resume`, each a row
   written and a return. The
   server is untouched but for tagging its requests `--by ui`: its
   worker still runs one job at a time, so the UI's own syncs overlap
   from 4b, and a job whose `datalib-dag` joined a CLI's loop has no run
   of its own until then. What was first written:
   `system/supervisor.sqlite` with `requests`, `pauses`, `steps` and
   `invocations`; the facts move there from `dag_state.json`. The round
   becomes a loop that reads intent from the store (`data_version`),
   runs until no request is open, and writes facts back. `datalib-dag`
   takes `runner-lock` and runs that loop, or — lock taken — writes its
   request and follows it as a client; the verbs `request`, `stop`,
   `pause`, `resume`. The server is untouched: its worker still spawns
   `datalib-dag`, which now joins whatever loop is running instead of
   failing on the lock, so two sources started a minute apart already
   run side by side (`data-sources-control.spec.ts`'s `test.fail()`
   comes off). With syncs overlapping, a run in the run store is a
   *busy period* — the loop going from idle to busy and back — and each
   request inside it is recorded against it (`sync_jobs.parent_job_id`
   for the server's jobs), so `status.rs`'s `effective_run` and
   `spoken_for`, and `dag_record`'s progress gate, learn "the job's run
   is its parent's". Startup closes the invocations a dead loop left
   open.

   **4b. The server runs the loop** (agreed 2026-09-23). *Built*
   (`http/src/supervisor.rs`, and `supervisor::host` in the library):
   Sync on a second source starts at once beside the first, instead of
   reading Queued until the first is done. What the Manage screen shows
   is otherwise unchanged — that is 4c. Where the build went past the
   brief below, or away from it:
   - A request for a step the loaded config lacks is left open for the
     next busy period, which loads the config again, when it arrives
     mid-period; one open as a period starts still fails. So a source
     *added* to the config while a sync runs waits for that sync; one
     the config already had starts beside it.
   - The loop tells its host when it takes a request on and when it is
     done with one (`RequestEvent`). A stopped request is done once the
     steps only it wanted have exited, so a job reads Stopping until
     then, as it did under the worker.
   - A step no open request wants any more settles its row at once (Up
     to date, Blocked) rather than when the busy period ends, which with
     several sources in one period can be a long way off.
   - A step killed at the end of its grace records as stopped, not
     failed. `CHILD_PIDS` / `kill_children` stay the CLI's exit path;
     the server's shutdown stops the loop (SIGINT to each step) and
     leaves the rest to each step's parent pipe. `worker_cancel` became
     `http_tests::sync_loop`.

   The brief, as agreed:

   - *The server holds `runner-lock` for its life and runs the loop in
     busy periods.* Idle, it polls the store's `data_version`; when a
     request is open it loads the config (once per busy period, as a job
     does today), mints a run id, starts a `RunStoreSink` and a
     `current_run`, and calls `Runner::serve` until no request is open.
     One busy period is one run. The step environment the binary builds
     today (`PATH` with the binary dir, `DATALIB_DAG_NOW`,
     `DATALIB_DAG_RUN_ID`, `RUST_LOG`, the checkpoint cadence) moves
     into the library so the binary and the server build it the same
     way. With the server up, every CLI sync is a client of it.
   - *`sync_jobs` stays as the UI's record, mirrored from requests.*
     `POST /api/sync/jobs` writes a request row and a job row with the
     same id. The host keeps the job row in step: `running` when the
     request is admitted, then `done` / `failed` / `canceled` from its
     outcome, and `parent_job_id` set to the busy period's run id. So
     the pill, Stop, banners and the job SSE frames keep working; the
     host sends the frames the worker used to. Cancel is
     `request_stop`. `status.rs` learns one thing: a job belongs to a
     run if its id **or its `parent_job_id`** is that run
     (`effective_run`, `spoken_for`), and `dag_record`'s progress gate
     keeps comparing run ids, which are now busy periods.
   - *Reset jobs run between busy periods.* They need the root to
     themselves and the server holds the lock, so the host runs a
     pending reset job when idle (`Runner::reset`).
   - *The cancel ladder moves into the library, per invocation.* The
     worker's three rungs (`worker.rs::next_stage`, aimed at the
     `datalib-dag` process) become: a stop sends SIGINT to the step's
     group, and SIGKILL after the grace if it is still there — in
     `run_subprocess`'s stop task, so the CLI gets it too.
     `http_tests::worker_cancel` is the test to keep green.
   - *"A sync is running" comes from the host, not the lock.*
     `DagRunInfo.live` (`http/src/lib.rs` ~1271),
     `usage::pipeline_is_running` (`usage.rs` ~489) and the e2e
     `settleRunner` (`ui/tests/e2e/grid-helpers.ts` ~524) read the
     host's "in a busy period"; with no server, they fall back to "the
     lock is held", which is a CLI run.
   - *`worker.rs` goes.* Its startup recovery is replaced by a better
     property: a request a dead server left open is still a row, and
     the next boot runs it. A job row still `running` from a dead server
     is set back to match its request.
   - *Steps die with the server already* (#710): each step watches the
     process that spawned it and stops itself if it dies, even by
     SIGKILL. Nothing to add, but `CHILD_PIDS` / `kill_children` become
     the host's shutdown path, not a process-wide "kill all".

   What the 2026-09-23 map of the server's sync path found, which a
   naive version would break silently:
   - Three places read "`runner-lock` is held" as "a sync is running"
     (above). A server holding it for good makes them always true.
   - "Job id = run id" is load-bearing in `status.rs`
     (`effective_run`, `spoken_for`, `claimed_by`), `dag_record`'s
     progress gate (`lib.rs` ~1287: activity chips and `fraction`
     silently go empty if it never matches), `live_run_id` /
     `last_run_id`, and the `run=` stamp in store commits.
   - The UI has two change signals: unnamed SSE job frames (the worker
     sends them; `live.ts`, `SyncProgressChrome.vue`,
     `SourcesCard.ce.vue`'s `mergeJob`) and `watch.rs`'s
     `table_changed` for `dag_state.json` and `runs.sqlite`. Rows stop
     updating live if either goes quiet.
   - The legacy `/sources` page (`SourcesView.vue`,
     `sources-view.spec.ts`) is a second job-driven UI.
   - `data-sources-control.spec.ts`'s `test.fail()` on "a source started
     during another's sync runs beside it" starts passing, which
     Playwright reports as a failure: take the marker off.
   - The Playwright suite only runs in an unfiltered `bazelisk test
     //...` or `bazelisk test //datalib/ui:e2e_test`; the hermetic line's
     `-external` filter drops it. Run it before pushing.

   **4c. Rows read the supervisor.** `steps.state` is what a Manage row
   says; `status.rs`'s inference (`reached_since`, `spoken_for`,
   `StatusFloor`) is deleted. `POST /api/requests`,
   `/api/requests/<id>/stop`, `/api/steps/<id>/{pause,resume}`, the
   Pause button, "Sync by claude" on a request an agent opened.
   `sync_jobs` goes, and so does the `/sources` page, a second
   job-driven UI, unless it is ported. `agent_user.md` is rewritten
   around the verbs.
5. **The UI**: per-row Sync and Pause, a requests panel with Stop per
   request and the wave under each, and Clear on
   a sink with the wording of §2.10. The help text is rewritten around rows, not runs.
6. **Shared sinks.** `writes`/`reads` in the config with the defaults
   of §2.1, the loader allowing two steps to name one sink, and the
   first provider that uses it: the email import beside the live pull.
   The reason §2.1 exists, landed last because everything before it is
   needed for it to be safe.
7. **Docs.** The dag README is rewritten around the tick;
   `step_protocol.md` gains `DATALIB_READS`; `streaming_steps*.md` move
   to `completed/` with a line saying the supervisor subsumed them;
   `join_running_sync.md` is deleted.
