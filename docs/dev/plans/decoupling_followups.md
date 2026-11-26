# What is left after the runner stopped knowing datalib-step

**Status: notes, partly built; checked against the tree 2026-11-20.**
The step protocol redesign (#1894) took `datalib-step` out of the
runner. This file lists what that redesign left alone on purpose, so it
is not forgotten. Done: items 1, 5a, 5b and 6, and of item 7 all but one
constant; item 5 is closed as not a coupling, and item 4 as Datalib's
own tool. Untouched: items 2, 3 and 8 (item 8 is decided). Of the
reconciler in section 10, steps 1 and 2 are done. Item 9 is half done.

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

### 1. Checking a step's params before a sync (done)

Checking params is the `--check` verb (`docs/dev/step_protocol.md`
§ Check): `datalib-pipeline check` and the app's save ask every step
whose program takes it (`datalib/backend/dag/src/check.rs`), and
`datalib-step` answers with its refusal or its warnings, the keys that
no longer do anything among them. The unattended rewrite `check_verb.rs`
adds the verb to an existing config's `datalib-step` entry. One step
is one process; up to eight are asked at once. Cross-step rules stay
with the step that can see the problem, as a warning when it runs.
`datalib-step check-params`, which did the same for a whole file, is
gone; `datalib-pipeline check` is the one way to ask.

### 2. Checking a connection

The wizard's "Check connection" and a picker's "Load" run
`datalib-step probe <type>` (`datalib/backend/http/src/probe.rs:1-4`,
`:84-108`), found by name next to the server
(`datalib/backend/http/src/binaries.rs:10-25`). A package's program
cannot offer a probe.

### 3. Adding a source writes datalib-step's steps

The source wizard knows `datalib-step`'s source types, their params and
the steps each one needs, and writes `command = "datalib-step"`
(`datalib/ui/src/config/sourceSteps.ts:225`,
`datalib/ui/src/config/catalog.ts`). A package already supplies its
own programs to the wizard (`datalib/ui/src/config/catalog.ts:243-245`).
The end state treats `datalib-step` the same way: a program that
describes what it offers, which the wizard reads, with no list of
Datalib's sources compiled into the UI.

### 4. The config upgrade rewrites datalib-step's steps (closed: kept as Datalib's tool)

`datalib-http` rewrites retired config shapes when it reads a config
(`datalib/backend/http/src/config_upgrade.rs:1-12`), through
`datalib_migrate_config`. Most of those rewrites are about
`datalib-step`'s own steps (`qmd_index`, the time-series steps, the
step `command`). The redesign adds more: `[[programs]]`, `reads`,
`locks` and the qmd `[[locks]]` on `datalib-step` steps. Knowing what
an old `datalib-step` step should become is `datalib-step`'s knowledge,
so the end state needs a way for a program to rewrite its own steps'
entries.

**Decided:** the rewrites stay in `datalib-migrate-config`, which is
Datalib's own migration tool and may know `datalib-step`
(`docs/dev/config_model.md` §"The retired shapes"). Many of them need
the whole file, not one program's entries: the time-series rewrite adds
a group and its steps, and the lock rename edits `[[locks]]`. A
protocol where a program rewrites only its own entries could not
express those. Lint check 24 covers only `datalib/backend/http/src`,
so it never reaches this crate. Revisit if packages start needing
rewrites of their own.

### 5. Every step's metrics are named `datalib_step_…` (closed: not a coupling)

`GET /metrics` names a step's series `datalib_step_<name>`, custom
steps' included (`datalib/backend/http/src/prometheus.rs:173`). That
reads as Prometheus's `<namespace>_<subsystem>_<name>`, the same form
as its siblings `datalib_tree_*`, `datalib_disk_*` and
`datalib_applet_*`: `step` is the subject of the series, not the
program `datalib-step`. `docs/dev/logging.md` now says so, and the
names stay, so no dashboard or alert breaks.

### 5a. The server hands two of datalib-step's secrets to every step (done)

The server used to hand `WHATSAPP_BACKUP_DECRYPTION_KEY` and
`SIGNAL_BACKUP_PASSPHRASE` to every step. Now the wizard always writes
the variable's name into a WhatsApp or Signal source's params
(`paramEntries` in `datalib/ui/src/config/sourceSteps.ts`), the
unattended rewrite `backup_secrets.rs` names the default in an existing
config that relies on it, and the server hands nothing by name.

### 5b. The runner hands every step three variables datalib-step reads (done)

`datalib_env` used to list, in `STEP_ALSO`, three variables only
`datalib-step` reads, and the runner handed them to every step. None is
set by anything Datalib ships:

- `DATALIB_CACHE_DIR` moves the fingerprint cache, and only tests set
  it; without it the cache goes under `XDG_CACHE_HOME` or `HOME`, which
  every step gets.
- `DATALIB_EMBED_THREADS` caps the embedder's threads, and only
  `.bazelrc` sets it. The server handed it to every applet as well
  (`APPLET_ALSO`), and no longer does.
- `MINDS_VIA_DESKTOP_URL_PREFIX` was a URL prefix minds published for
  some requests. minds no longer publishes it and the route behind it is
  gone, so the code that read it is deleted.

The first two are now test switches: a test names them in
`DATALIB_CHILD_ENV` (`.bazelrc` does so for every bazel test). A
program that needs a variable of its own has no way to say so in its
`[[programs]]` entry yet; that waits for a package that needs one.

## Where datalib-http and datalib-pipeline are not yet separate

### 6. The server runs the loop in its own process (done)

`datalib-http` keeps `datalib-pipeline serve` running as a child
(`run` in `datalib/backend/http/src/supervisor.rs`), restarts it with a
backoff when it dies, and says on every Sync button when it cannot
find one or it keeps dying. It still links the runner's crate, as a
client library: config parsing, request and wipe rows, the record. It
talks to the child only through the request rows and the record in
`system/supervisor.sqlite`, the announcements in
`system/supervisor-listeners/`, the run store, `config.toml`, and the
`ServeEvent` lines on the child's stderr. A reset, purge or free-space
request is no longer refused when another process runs the loop. How
it behaves is in `datalib/backend/dag/README.md` §"Two locks, two
files".

### 7. Datalib's constants live in the runner's crate (mostly done)

`DIFF_GROUP_TYPE` and `TIME_SERIES_INDEX_GROUP` moved to
`datalib/backend/http/src/step_program.rs`, and the app's starter config
to `datalib/backend/http/src/starter.rs`; `datalib_dag::starter` now
holds only `datalib-pipeline init`'s, which has no steps. Left:
`RESERVED_APPLET_ID` (`datalib/backend/dag/src/config.rs`), which the
runner enforces on `[[applets]]` ids and the server mirrors as
`USER_NAMESPACE` (`datalib/backend/http/src/frontend.rs`). It goes with
item 8.

### 8. `[[applets]]` is in the runner's config but the runner never runs one

The gateway in `datalib-http` starts applets
(`datalib/backend/http/src/applets.rs:91`), using `AppletEntry` from the
runner's crate (`datalib/backend/dag/src/config.rs:357`).

**Decided:** the model of the config file moves out of the runner's
crate into a crate of its own, which the runner, the server,
`datalib-step` and `datalib-migrate-config` all link, and `[[applets]]`
stays in it. `datalib_dag::config` is more than the runner's: it is the
model of the file every program reads (`datalib-step` links it in
`datalib/backend/datalib_step/src/append.rs` and `check.rs`). The move
takes `config`, `diagnostics`, `written` and `config_order`. The two
options it was chosen over were the runner skipping `[[applets]]` while
the server parses it, and applets moving to a file of their own.

An applet now gets its params in `DATALIB_DAG_PARAMS`, as a step does,
rather than in a file named by `--params-file`. The helper that writes
that file, which only the probe still uses, has moved out of the runner's
crate to `datalib/backend/http/src/params_file.rs`, and goes once the
probe takes its params the same way (items 2 and 3). `applet_type`
(`datalib/backend/dag/src/written.rs`), which knows that
`datalib-applet <type>` names an applet's type, moves with the rest of
the config file's lenient reading.

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
`datalib/backend/http/src` outside tests. The runner's half is check 23
in `scripts/lint_repo.py`. The server's is check 24, which for now lets
the string stand in the files of the items still open (2 and 3), named
in `_SERVER_STEP_PROGRAM_FILES`, and refuses a listed file that no
longer needs it; the list empties as those items land.

## 10. The runner's runtime model

The runner began as a batch runner and became a supervisor by being
wrapped rather than rewritten. This is a review of how it runs, and the
design it would have if it were written today. It was written
2026-11-17 and checked against the tree on 2026-11-20, after the server
started the loop as a child (item 6); the section "How it works now"
describes the tree on that date.

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

- **A pure core.** `tick()` (`datalib/backend/dag/src/supervisor/tick.rs:239`)
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
  (`datalib/backend/dag/src/supervisor/serve.rs`), which
  `datalib-pipeline serve` runs (the server's child), and the CLI's sync
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
    answer as a `ServeEvent` on stderr, which the server keeps in memory
    (`Upgrade`, `datalib/backend/http/src/supervisor.rs`); the CLI's
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
   `datalib/backend/dag/src/bin/datalib_pipeline.rs:577`), and maintenance
   runs through two paths with their own tests. The prepare pass's
   progress is in the server's memory rather than the store, carried
   from the child as `ServeEvent` lines: a client other than the
   server cannot see it.
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

1. **One loop (done).** `serve` and `run_idle` are one loop that does
   not exit, `Runner::run_forever`. The busy period is kept only as the
   run store's grouping, which the host records through
   `supervisor::period::RunPeriods`. `settle`, `run_idle`, the old
   `Periods` trait and the at-rest tick are gone. The CLI's sync still
   serves one period and exits (`Runner::serve`), until step 3 makes it
   a client.
2. **Key by step id (done).** The loop's own state is one slot per step
   id (`round.rs`'s `Slots`), holding what an invocation read by producer
   id; the facts the tick takes are laid out by index from the record and
   those slots at every tick. A swap replaces the graph and the lock shape,
   and finishes what a dropped step had ended. `carry`, `remap_consumed`,
   `Slot::remapped`, `QueueLedger::remap` and `remap_by_step` are gone,
   and the queue ledger is keyed by step id.
3. **Everything goes through the mailbox.** Done so far: the prepare
   pass runs inside the loop and holds only what it has not reached
   (open question 3). Still to do: turn `wipes` into `jobs`
   and add prepare, with its progress in `job_steps` instead of the
   server's memory. Make the CLI's reset and shrink write jobs. Put
   both hosts on `supervisor::run`. Item 6 did not wait for this: the
   prepare pass's progress reaches the server as `ServeEvent` lines.
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
   has not reached? (answered: only those.)** The pass runs inside a
   host's loop, one step at a time beside everything else. A step it has
   not asked, and what reads one, waits (`Wait::Prepare`), and Sync stays
   offered. The CLI's sync still asks every step first, until it becomes
   a client.
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
