# The supervisor: steps as managed processes, not as a batch run

**Status: greenfield proposal (2026-09-19, revised 2026-09-21); its
slice 0 is built, the rest is not.** This is the alternative to
[`join_running_sync.md`](join_running_sync.md), which patches the runner
we have. Both start from the same measurement (§0 there). This one asks
what we would build if the UI's needs came first. §1 describes the tree
as it stands at `692f59bd`, after #600 (a writer's `open` discards the
working set; no commit on SIGINT) and #606 (a stop ends a download at
its next consistent point) landed; nothing else here describes the
tree. Where this doc and the tree disagree, the tree wins.

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
| a doltlite store (raw, render) | head commit | `dolt_commit` at a seal; readers pin a commit | `flock` on the sibling `.lock` | the next `open` discards the working set |
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

For a doltlite sink the version is read by the supervisor after any
writer finishes (`datalib_history` already reads a store's log without
linking `etl`). The step's `outcome` line keeps working unchanged; it
is just no longer the *only* way a sink's version can move.

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

**The tick is the functional core; everything around it is the
shell.** That is the pattern [`style.md` § Functional core, imperative
shell](../style.md) asks for, and this is the one place in the tree
where it matters most. The tick takes values — the graph, the open
requests and pauses, every sink's version, which steps are running —
and returns values: the state each row should show, and the list of
steps to start with the versions they consume. It calls nothing that
touches the world. The shell around it is small and dumb: it turns an
event (a step exited, a checkpoint line arrived, a request row was
inserted, the config file changed) into an update to those values,
calls the tick, and does what the tick said — spawns the subprocess,
writes the `steps` and `invocations` rows, pushes the frame.

What that buys is the thing the current runner never had: every
scheduling claim in this document becomes a synchronous test with no
tokio, no tempdir and no polling. "A tick with stale steps and no open
request starts nothing", "a checkpoint mid-pass owes exactly one more
pass", "Stop on one request leaves the fan-in in the other's scope" —
each is a few values in, a list of starts out, and any interleaving of
events you can think of is a test you can write in the order you
thought of it. `scheduler.rs` today keeps the same facts as a dozen
parallel `Vec<bool>`s mutated between `JoinSet::spawn` and
`state.save`, and every one of its streaming tests has to run real
tasks and wait on a flag to observe the state machine. The
`Decision::{Run, Skip, Block}` enum there is already the shape of the
tick's output; the supervisor finishes the thought.

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
starts from the beginning. `--reset-and-redownload` becomes
`clear` followed by `request`, and the ingest code loses its
`reset_and_redownload` branch: a download never wipes, it only
downloads.

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
  everything, the batch CLI, a schedule on a wide source) does reach
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
| an agent steering it | `POST /api/sync/jobs`, then read three stores | the same four verbs the buttons use; one plain-SQLite store, with `by` |
| what it keeps | everything; adds ~7 slices | step protocol, graph, versions, retry, run store, etl locks |
| what it removes | `parent_job_id` | `Runner::run`, `dag_state.json` (the half `logs_and_metrics` left), `worker.rs`, `sync_jobs`, `status.rs`'s inference, `runner-lock` |
| size | ~1.5 weeks | ~4 weeks, most of it deleting |
| risk | scheduler recount bugs | the two-process class of test |

The join is a patch to a batch runner. This is the thing the UI was
already pretending the batch runner was.

## 5. Order of work, if this is the way

Each slice lands green and the app works after each.

0. ~~**The rescue commit goes**~~ **Done: #600, #606.** `RawDb::open`
   discards a dirty working set instead of sealing it, and the
   interrupt commit hook is gone; SIGINT raises a stop flag and the
   download ends at its next consistent point with a real final commit.
   Taking the rescue away exposed that `RawStoreSession::finish` never
   committed the blob CAS — the next run's rescue had been doing it —
   and that jmap saved its state token before its enumeration finished.
   Both fixed there.
1. **Sinks in the graph.** `writes`/`reads` in the config with the
   defaults above; `Graph` bipartite; the loader allows a shared sink.
   No scheduler change yet: the current runner treats a shared sink as
   a diagnostic-level warning and runs as now. Tests: today's configs
   load identically; a shared sink loads.
2. **Sink versions from the sink.** A doltlite sink's version is its
   head commit, read by the framework; the step's report is checked
   against it in tests, then becomes optional for doltlite sinks. The
   qmd index gets a `versions` row written in the transaction that
   updates it. A plain tree keeps the tree hash, computed on writer
   completion and cached; the supervisor learns which sinks' readers do
   not pin, so it never starts a writer on one while a reader runs.
3. **The supervisor library**, batch mode only: the tick of §2.3 as a
   pure function over values, with its tests written first and
   synchronous — the two hazards in §3 that say "the first test
   written" are the first two — then the thin shell that feeds it
   events and runs its starts, with one request rooted at every
   source, run until it closes. It passes the scheduler's existing
   tests re-expressed against requests and invocations (the semantics
   they pin — subset sync, not-selected history, unselected trees
   never hashed — are exactly what scope means, and hold), and most
   of them stop needing tokio to say so. `datalib-dag` switches to
   it; the fixture genrule is the proof.
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
