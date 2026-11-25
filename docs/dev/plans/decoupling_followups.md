# What is left after the runner stopped knowing datalib-step

**Status: notes, 2026-11-18.** The step protocol redesign (#1894) took
`datalib-step` out of the runner. This file lists what that redesign leaves alone on
purpose, so it is not forgotten.

## The end state

`datalib-http`, `datalib-pipeline` and `datalib-step` are three separate
programs that happen to live in one repo. Each talks to the others only
through a documented protocol, and none of them gives another one
treatment a third-party program could not get the same way:

- **`datalib-pipeline` ↔ a step program**: the step protocol
  (`docs/dev/step_protocol.md`). `datalib-step` is one program that
  speaks it.
- **`datalib-http` ↔ `datalib-pipeline`**: the runner's files and
  announcements (item 6). `datalib-http` is one client of the runner,
  like the CLI.
- **`datalib-http` ↔ a step program**: whatever the app needs from a
  program to offer it in the UI (adding a source, checking a
  connection, checking params). Today this is `datalib-step`'s private
  subcommands; it should be a protocol a package's program can speak
  too.

So `datalib-http` should not privilege `datalib-step` either. Today it
does in each of items 1 to 5.

## Where datalib-http knows datalib-step

### 1. Checking a step's params before a sync

**Now.** `datalib-pipeline check` runs `datalib-step check-params` on a
config that has built-in ingest or render steps
(`datalib/backend/dag/src/bin/datalib_pipeline.rs:450-465`), and
`datalib-http` does the same on every save
(`datalib/backend/http/src/params_check.rs:43`, `:55`). The redesign
removes the runner's call and keeps `datalib-http`'s, for now.

**What is wrong.** Only `datalib-step`'s steps get their params checked
before they run; a custom step's mistake shows up only when it fails.
A person who edits `config.toml` by hand and runs `datalib-pipeline check`
no longer sees a params error before the first sync.

**Shape discussed.** A way for any program to check one step's params,
with optional batching so a save does not start one process per step.
Cross-step rules, such as "`qmd_aggregator` must read every
`keyword_index`" (`datalib/backend/dag/src/config.rs:1680-1694`), do
not go through it: the step that can see the problem reports it as a
warning when it runs.

**Also lost: the warnings for params that do nothing.** The runner used
to warn, at the line, about a `datalib-step` step's params that no
longer do anything: `period` and `backup.period` (a chat is one
document now), `common.always_clear_before_ingest`, linkedin's
`export.fetch_photos` and email's `gmail.request_concurrency`. The
list was the runner's (`INERT_PARAM_PATHS` in `config.rs` before this
change); `datalib-step` still drops those keys before it parses
(`INERT_PARAM_PATHS` in `datalib/backend/datalib_step/src/methods.rs`),
so the step runs, but nothing tells the person the line can go. The
same check-params door is where they belong, as warnings.

### 2. Checking a connection

The wizard's "Check connection" and a picker's "Load" run
`datalib-step probe <type>` (`datalib/backend/http/src/probe.rs:1-4`,
`:84-91`), found by name next to the server
(`datalib/backend/http/src/binaries.rs:15-40`). A package's program
cannot offer a probe.

### 3. Adding a source writes datalib-step's steps

The source wizard knows `datalib-step`'s source types, their params and
the steps each one needs, and writes `command = "datalib-step"`
(`datalib/ui/src/config/sourceSteps.ts:221-223`,
`datalib/ui/src/config/catalog.ts`). A package already supplies its
own programs to the wizard (`datalib/ui/src/config/catalog.ts:242`).
The end state treats `datalib-step` the same way: a program that
describes what it offers, which the wizard reads, with no list of
Datalib's sources compiled into the UI.

### 4. The config upgrade rewrites datalib-step's steps

`datalib-http` rewrites retired config shapes when it reads a config
(`datalib/backend/http/src/config_upgrade.rs:1-12`), through
`datalib_migrate_config`. Most of those rewrites are about
`datalib-step`'s own steps (`qmd_index`, the time-series steps, the
step `command`). The redesign adds more: `[[programs]]`, `reads`,
`locks` and the qmd `[[locks]]` on `datalib-step` steps. Knowing what
an old `datalib-step` step should become is `datalib-step`'s knowledge,
so the end state needs a way for a program to rewrite its own steps'
entries.

### 5. Every step's metrics are named `datalib_step_…`

`GET /metrics` prefixes every series with `datalib_step_`, custom
steps' included (`datalib/backend/http/src/prometheus.rs:173`, `:194`,
`:208`). The name says the series came from `datalib-step` when it came
from any step. A rename breaks a person's dashboards and alerts, so the
old names have to be kept for a while.

### 5a. The server hands two of datalib-step's secrets to every step

`WHATSAPP_BACKUP_DECRYPTION_KEY` and `SIGNAL_BACKUP_PASSPHRASE`, when the
server was started with them, go to every step it runs
(`backup_secrets`, `datalib/backend/http/src/supervisor.rs`, handed to
the loop as `ServeOptions::extra_env`), because the
wizard may write a WhatsApp or Signal source without naming the
variable in its params and `datalib-step` then falls back to these
names. The config upgrade names them in an existing config's params,
which is the protocol's own way. The end state: the wizard always
writes the variable's name into the params, and the server hands
nothing by name.

## Where datalib-http and datalib-pipeline are not yet separate

### 6. The server runs the loop in its own process

**Now.** `datalib-http` does not start `datalib-pipeline`. It links the
runner's crate (`datalib/backend/http/BUILD.bazel:122`) and calls
`datalib_dag::supervisor::serve::run` in-process
(`run`, `datalib/backend/http/src/supervisor.rs`): the same loop
`datalib-pipeline serve` runs. That takes the runner lock, closes what a dead
loop left, runs the prepare pass, then serves every request and wipe
until stopped. What the server needs and no store holds, the loop says
as `ServeEvent`s, which the server turns into its `Upgrade` (the
prepare pass `/api/config` reports) and its `busy` flag.

**Wanted.** `datalib-http` starts `datalib-pipeline serve` as a child
process. It may keep linking the crate as a client library (config
parsing, writing request rows, reading the record); what goes is the
loop running in its process. It talks to the child only through:
- the request rows in `system/supervisor.sqlite`;
- the record in the same file;
- the announcements in `system/supervisor-listeners/`;
- the run store `system/runs/runs.sqlite`;
- `config.toml`;
- the child's stderr, one `ServeEvent` per line: whether it holds the
  lock, the prepare pass's progress, each busy period, and each write
  it makes to the run store. That last one replaces the in-process
  hook (`datalib_runs::after_every_write`, wired in
  `datalib/backend/http/src/boot.rs:122`), which no longer fires once
  the writes are another process's.

**What is left to decide or build.**
- The server finds the binary the way it finds `datalib-step`
  (`datalib/backend/http/src/binaries.rs`), sends SIGTERM on shutdown
  and kills the child after its grace, and restarts a child that died,
  logging each restart at ERROR.
- The two backup secrets reach the steps through `ServeOptions::extra_env`
  in-process. A child is handed only an allowlist of the server's
  environment, so they need a way across: `DATALIB_CHILD_ENV`, or item
  5a done first.
- The reset, purge and free-space handlers refuse when another process
  runs the loop (`SyncControl::wipe`). With the loop always in another
  process that check goes: any loop carries out a wipe row.
- The tests that build a `SyncControl` directly
  (`datalib/backend/http/tests/http_tests/support.rs:30`) and need a
  loop take the `datalib-pipeline` binary as a `data` dep.
- The log card reads "the server's lines"; the child's own lines are a
  `dag` process's, so check the card still shows the loop's.

### 7. Datalib's constants live in the runner's crate

`datalib-http` reads these from `datalib_dag::config`, so they stay in
the runner after the runner stops using them. They move out of the
runner's crate.
- `DIFF_GROUP_TYPE` (`datalib/backend/dag/src/config.rs:380`), used at
  `datalib/backend/http/src/manage/mod.rs:1427`.
- `TIME_SERIES_INDEX_GROUP` (`datalib/backend/dag/src/config.rs:754`),
  used at `datalib/backend/http/src/manage/mod.rs:1254`.
- `RESERVED_APPLET_ID` (`datalib/backend/dag/src/config.rs:899`), used
  by `datalib/backend/http/src/frontend.rs:17`.
- The starter config, `datalib_dag::starter`
  (`datalib/backend/http/src/lib.rs:23`, `:1179`;
  `datalib/backend/http/src/main.rs:137`).

### 8. `[[applets]]` is in the runner's config but the runner never runs one

The gateway in `datalib-http` starts applets
(`datalib/backend/http/src/applets.rs:91`), using `AppletEntry` from the
runner's crate (`datalib/backend/dag/src/config.rs:430`). Either the
runner treats `[[applets]]` as a section that belongs to another
program and only skips it, or applets move to a file of their own.

Two helpers for applets also still live in the runner's crate, though
the runner no longer uses them: `write_params_file` and
`PARAMS_FILE_FLAG` (`datalib/backend/dag/src/subprocess.rs`), which the
gateway and the probe use to hand a program its params in a file, and
`applet_type` (`datalib/backend/dag/src/written.rs`), which knows that
`datalib-applet <type>` names an applet's type. They move with the
applets.

## When the decoupling is done

### 9. State the principle in `AGENTS.md`

Once items 1 to 8 and the step protocol redesign have landed, add the
rule to `AGENTS.md`, where every contributor reads it, and not before:
a rule the tree does not yet follow would mislead the next person who
reads it. The text:

> **datalib-http, datalib-pipeline and datalib-step are decoupled.** They
> are three separate programs that happen to live in one repo. Each
> speaks to the others only through a documented protocol, and none
> gives another treatment a third-party program could not get the same
> way. New behaviour is a protocol extension (a config key, a verb, an
> event or an environment variable) that any program may adopt, and
> the program that needs it is then taught to use it.

Add the check that keeps it true at the same time: `//:lint_repo`
refuses the string `datalib-step` in `datalib/backend/dag/src` and in
`datalib/backend/http/src` outside tests.

## 10. The runner's runtime model

The runner began as a batch runner and became a supervisor by being
wrapped rather than rewritten. This is a review of how it runs, and the
design it would have if it were written today. It was written
2026-11-17 and checked against the tree on 2026-11-19, after the step
protocol landed (#1894); the section "How it works now" describes the
tree on that date. Item 6 above (the server running the loop
in-process) is part of it.

### Why

`datalib-pipeline` started as a batch runner, like `make`: read the config,
run what is stale once, report, exit. It is now a supervisor: the app's
server holds it for its whole life, people open and stop syncs at any
time, the config is edited while steps run, and a download's consumers
run each time it seals a batch. The supervisor was built by wrapping
the batch runner rather than replacing it, and parts of the batch
design are still load-bearing. This document lists those parts and
proposes the design we would pick if we started from the requirements.

### What the runner has to do

1. Keep each step's output up to date with its inputs and its
   definition, judged by versions the steps report.
2. Take requests at any time: sync these steps and what reads them,
   stop a sync, turn a step off, reset, purge, compact, collect.
3. Take a config edit while steps run.
4. Run a consumer on each batch a streaming producer seals.
5. Keep steps apart: locks, budgets, the disk floor.
6. Let only one process run the loop per data root, and let any other
   process (the CLI, the server's HTTP handlers) ask it for work.
7. Survive a crash: a dead loop's leftovers are closed by the next.
8. Show what is happening and what happened: each step's state, logs,
   metrics, history.
9. Each time the loop starts, before a step runs, ask each step whose
   program declares the `prepare` verb to make what it stored usable
   by this build (`docs/dev/step_protocol.md` § Prepare).

### How it works now

Paths are relative to the repo root.

- **A pure core.** `tick()` (`datalib/backend/dag/src/supervisor/tick.rs:200`)
  takes the graph's shape, the open requests and switches, and the
  facts, and returns every step's state plus the starts, stops and
  request closures to make. This is already the design we would want.
- **A busy loop.** `Runner::serve`
  (`datalib/backend/dag/src/supervisor/round.rs:213-805`, one function
  of about 600 lines) runs from the moment a request is taken on until
  none is left. That stretch is a **run**: it gets a run id, a pinned
  `DATALIB_DAG_NOW`, a fixed step environment, a row in the run store,
  and an end-of-run `RunReport`.
- **An idle loop.** Between runs, `host::run_idle`
  (`datalib/backend/dag/src/supervisor/host.rs:41`) waits for a request
  or a wipe and starts a busy period. If a switch or the config moves
  while idle, it calls `Runner::settle`
  (`datalib/backend/dag/src/supervisor/round.rs:1094`), which runs one
  tick with nothing open so the record shows the change. Only the
  server and the tests run `run_idle`; the CLI calls `serve` once and
  exits.
- **Two hosts.** `supervisor::serve::run`
  (`datalib/backend/dag/src/supervisor/serve.rs`), which the server
  calls in-process and `datalib-pipeline serve` runs, and the CLI's sync
  command (`datalib/backend/dag/src/bin/datalib_pipeline.rs`). Each takes
  the lock, closes what a dead loop left (`host::take_over`,
  `host.rs:277`), runs the prepare pass, builds the step environment
  (`host::step_env`, `host.rs:116`), opens the run store and calls
  `serve`. They do these things in their own code, and not identically.
- **Three ways to do maintenance.**
  - In the app, reset, purge, compact and collect are rows in the
    `wipes` table that the loop carries out (`carry_out_wipes`,
    `round.rs:933`).
  - From the CLI, `reset` and `free-disk-space` take the lock
    themselves and call `Runner::reset` and `Runner::shrink` directly
    (`datalib/backend/dag/src/scheduler.rs:150`, `:274`), outside any
    loop.
  - The prepare pass: each host calls `Runner::prepare`
    (`scheduler.rs:234`) before its first loop, at every start. A step
    that refuses is turned off with the marker `REFUSED_PREPARE`
    (`scheduler.rs:399`), and a later start that does not refuse turns
    it back on. `serve::run` asks one step at a time and says each
    answer as a `ServeEvent`, which the server keeps in memory
    (`Upgrade`, `datalib/backend/http/src/supervisor.rs:37`); the CLI's
    sync runs it in one call and prints a line per step that needs
    attention. Either
    way the pass gets its own run in the run store, of kind
    `RunKind::Migrate` (`host::prepare_start`, `host.rs:254`).

### Relics of the batch design

1. **The run is a busy period.** Whether two syncs are one run or two
   depends on timing alone: opened one second apart, they share a run;
   opened one second after the first ends, they don't. Many things are
   keyed on that accidental unit:
   - the run store's rows and logs;
   - `last_run` and `current_run.states` in the record
     (`datalib/backend/dag/src/supervisor/record.rs:18-55`);
   - `DATALIB_DAG_NOW`, which is pinned at the start of the run. So a
     request that joins a run hours after it began gets an old "now".
   - the step environment, which is fixed for the run. So a config
     edit to `log_level` or `[env]` does not reach steps until the run
     ends (`datalib/backend/dag/README.md:239-241`).
2. **Busy and idle are two loops.** Because the busy loop exits, the
   idle side needs its own way to update the record (`settle`), its
   own config-change handling, and a `Periods` trait for the hosts to
   plug into (`host.rs:23`). At the end of every run, `serve` also
   runs an extra "at rest" tick (`round.rs:750-765`) to leave the
   record right for the idle period.
3. **A report at the end of a batch.** `RunReport`, `StepReport` and
   the `RunSummary` event (`round.rs:767-804`) exist so the CLI can
   print one line per step when the run finishes. In a loop that never
   has to finish, this report is about the busy period, not about
   anything the person asked for.
4. **"A step finishes once per run."** Under streaming a step is
   invoked again for each seal, so the loop separates a *pass* (one
   process) from the step *finishing* (once its producers settle). It
   holds each ended pass in `Slot::ended` until upstream is quiet
   (`round.rs:484-555`), emits `PassEnd` and `StepFinish` separately,
   and counts `failed_passes` so a failure that a later pass hid is
   still reported. All of this exists to make the result look like a
   single batch step.
5. **State kept by graph index, then remapped.** Loop state is kept in
   vectors indexed by position in the graph. A config swap has to
   rebuild every vector with `carry`, `remap_consumed`,
   `Slot::remapped` and `QueueLedger::remap` (`round.rs:305-405`), and
   `LastWanted::Blocked` stores a step id just so it survives the swap
   (`round.rs:102-134`).
6. **Too many words for a step's state.** There are seven
   vocabularies, each from a different stage of the design:
   - `RunState` (`datalib/backend/dag/src/run_state.rs:28`);
   - `StepStatus` (`datalib/backend/dag/src/scheduler.rs:453`);
   - `StepState` / `StateKind` (`tick.rs:149`);
   - `LastWanted` (`round.rs:102`);
   - `LiveState` (`datalib/backend/runs/src/lib.rs:61`);
   - `RequestStepPhase` (`datalib/backend/http/src/request_progress.rs:24`);
   - `PassState` (`datalib/backend/dag/src/supervisor/pass.rs:26`),
     a step's place in the prepare pass or a compaction, plus the
     server's `Upgrade::migrating` flag beside it.

   Only two questions need answers: what state is a step in now (the
   tick's output), and how did a process end (an invocation's
   outcome). The word "migrate" also outlived the verb: the prepare
   pass's run is still `RunKind::Migrate`.
7. **Operations outside the loop.** The CLI's reset and shrink, and
   both hosts' prepare pass, call into the runner outside the loop and
   need the root to themselves. So `reset` and `free-disk-space`
   are refused while the app is up (`acquire_or_explain`,
   `datalib/backend/dag/src/bin/datalib_pipeline.rs:542`), and maintenance
   runs through two paths with their own tests. The prepare pass's
   progress is in the server's memory rather than the store, so item 6
   cannot carry it to a child process as it stands.
8. **A builder for a batch runner.** `Runner::new(..).sink(..).child_env(..).stop_on(..).reload_from(..)`
   (`datalib/backend/dag/src/scheduler.rs:90-140`) is configured once
   per run and thrown away. `StepRun::InProcess`
   (`datalib/backend/dag/src/step.rs:224`) and `RetryPolicy`
   (`scheduler.rs:27`) are part of the same API; `InProcess` is now
   used only by tests.

### The design from scratch

**A reconciler**: one loop, always running while its process holds
`runner-lock`, that compares what is wanted with what is, and acts on
the difference. Kubernetes controllers work this way. Most of the
pieces already exist; the change is mainly what gets removed.

#### State

`system/supervisor.sqlite` holds two kinds of rows.

| kind | tables | written by |
|---|---|---|
| **wanted** | `requests`, `turned_off`, `jobs` (reset, purge, compact, collect, prepare) | anyone |
| **observed** | `steps` (reads, fingerprint, last success, needs-rerun), `sinks` (versions), `invocations`, `job_steps` | the loop only |

`jobs` replaces `wipes` and also holds the prepare pass; `job_steps`
replaces `wipe_steps` and the server's in-memory `Upgrade`. The config
file is wanted state too: the loop reads it on `config changed`.

#### The loop

```
loop {
    wait for: an announcement, a child's exit, a seal, the disk timer, stop
    read what moved (mailbox rows, config, disk)
    decision = tick(graph, wanted, observed)      // pure, as now
    act: start and stop processes, run jobs, close requests
    save observed state and each step's state
}
```

The loop never exits on its own, so idle is just a tick that starts
nothing. Several things then have no reason to exist: `run_idle`,
`settle`, `Periods`, the at-rest tick, and the busy/idle handover.

#### Identity

All loop state is keyed by step id, and an invocation in flight holds
a copy of the spec it started with. A config swap replaces the graph
value and nothing else, so the remap code goes. A running step that
the new config drops keeps running from its own spec until it ends,
which is what happens now, without any special case.

#### History: invocations and requests, no runs

- An **invocation** is one process. Its row has the step, the spec's
  fingerprint, what it read, start and end, outcome, and exit status.
  Logs and metrics are keyed by invocation.
- A **request** is one thing a person or agent asked for. Its row
  lists the invocations started for it.
- "Last synced", "what this sync did" and "this step's log" are all
  queries over those two tables.

A **run** in the UI's sense, if we keep the word, is a request.
`DATALIB_DAG_RUN_ID` becomes `DATALIB_DAG_INVOCATION_ID`; a step that
wants a group id gets its request ids.

Each invocation's environment is built when it starts, from the
config as it is then. `DATALIB_DAG_NOW` is open question 1.

#### One host, and clients

- `datalib_dag::supervisor::run(root, stop)` does everything a host
  does now: take the lock, take over from a dead loop, queue the
  prepare job, and loop. The server calls it, and so does
  `datalib-pipeline serve`.
- Every other CLI verb is a client. `datalib-pipeline sync`, `reset` and
  `free-disk-space` write a row and follow it to the end. When no
  loop is running, the CLI starts one inside its own process and stops
  it once nothing is open. So `reset` no longer needs the root to
  itself.
- The prepare pass is a job that the loop queues each time it starts
  (open question 3).

#### Streaming without passes

There is no "the step finished". Each invocation ends with an
outcome, and a step is `fresh` once it is up to date and nothing
upstream of it is running or due (the tick already computes this
for the Manage row). A sync's result is its request's outcome: `done`
if every step in its scope is fresh, `failed` if one is failed or
blocked. That removes `Ended`, `PassEnd` versus `StepFinish`, and
`failed_passes`. A failed invocation that a later one recovered from
remains in the history as a failed invocation.

#### Retries

Retries move from inside an invocation (`invoke_with_retry`,
`scheduler.rs:850`) into the tick. A failed invocation counts as an
attempt, and backoff becomes a reason to wait (`Wait::Backoff(until)`).
One consequence: a step waiting out a backoff no longer holds its lock
or shows as Running.

### What stays

- `tick()` and the rules in `datalib/backend/dag/README.md`
  §"What the loop runs, and what makes a step stale", unchanged.
- Versions, fingerprints, sinks and the P1 and P2 rules for sinks.
- Locks, the disk floor, announcements over FIFOs, `runner-lock`.
- The step protocol as `docs/dev/step_protocol.md` states it: declared
  verbs, `--prepare` at every start and its four answers.
- The puppet harness (`//datalib/backend/dag:supervisor_harness_test`).
  It already drives the loop as a person would through the store,
  which makes it the right test to change the loop under.

### Order of work

Each step ships alone, and the harness stays green throughout.

1. **One loop.** Merge `serve` and `run_idle` into a loop that does
   not exit. Keep the busy period only as the run store's grouping for
   now. This removes `settle`, `Periods` and the at-rest tick.
2. **Key by step id.** This removes the remap code.
3. **Everything goes through the mailbox.** Turn `wipes` into `jobs`
   and add prepare, with its progress in `job_steps` instead of the
   server's memory. Make the CLI's reset and shrink write jobs. Put
   both hosts on `supervisor::run`. This is what item 6 needs.
4. **Invocations and requests replace runs** in the run store, the
   record, the server's routes and the UI. This is the largest step.
   The run store is history, not data a person built, so the old rows
   can stay readable without being migrated; that needs deciding.
5. **One state vocabulary per question.** Drop `RunReport`,
   `StepStatus`, `LastWanted`, `PassEnd`/`StepFinish`,
   `failed_passes` and `RunKind::Migrate`.
6. **Retries in the tick.**

The step protocol (#1894) already made verbs something a program
declares (`[[programs]].verbs`) and sends `--prepare` at every start.
Here those verbs become jobs: the loop runs a prepare job by invoking
each step whose program declares the verb, as `Runner::prepare` does
now outside the loop.

### Open questions

1. **What `DATALIB_DAG_NOW` should mean.** Its purpose is that every
   output stamped in one sync agrees on the time. Per request is the
   natural replacement. A step serving two requests then needs a rule
   for which one's time applies, such as the oldest open request it
   serves.
2. **Does the UI need runs at all?** The Runs card and the log card
   group by run. Grouping by request matches what a person did; grouping
   by invocation matches what a process did. Before we choose, we need
   to know which one people look at.
3. **Does the prepare pass block every request, or only the steps it
   has not reached?** Now it blocks everything: the server disables
   every Sync (`ManageResponse::sync_paused`,
   `datalib/backend/http/src/manage/mod.rs:266`) and a request opened
   another way waits for the pass. Since #1894 the pass runs at every
   start, not only after an upgrade, so that wait is paid at every
   launch. As a job, it could hold only the steps it has yet to ask and
   what reads them, the way a wipe holds only its targets.
4. **Retries and locks.** With retries in the tick, a step backing off
   releases its locks. That is better for a shared rate limit, but it
   lets another step take the slot in between. Check whether any
   current step relies on keeping its lock across a retry.
5. **One process per streaming consumer, instead of one per seal.**
   A step that reads a streaming input could be started once and stay
   alive until its inputs finish, the way `producer | consumer` works:
   it opts in (`streams_input = true`), gets a pipe on stdin with one
   NDJSON line per new input version, and finishes when stdin reaches
   end of file, which means every input has finished. That removes
   passes (relic 4) and makes "settled" the same as "every producer
   has exited", so `start = "after_inputs_settle"` goes too.
   `datalib-step`'s render can do it, since its work already follows
   its own cursor. Open: a consumer waiting between seals is a live
   process, so it holds its locks and memory. Either streaming
   consumers take no budget slot, or the protocol gains a way to say
   "idle, release my slot". Also, each source's `embed` reads the grid
   index's files (`reads = "files"`, `configs/dag_example.toml:132`),
   and no writer of those may run beside it. With the index kept alive
   for a whole sync, `embed` would start only once the index exits, so
   search by meaning would fill in only at the end of a long first
   sync.
