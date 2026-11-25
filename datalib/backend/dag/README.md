# `datalib-pipeline` — the runner

Reads a `config.toml`, builds a DAG from it, and runs the steps. This file
holds the rules you cannot recover by reading the code; the contract a
step author needs is
[`docs/dev/step_protocol.md`](../../../docs/dev/step_protocol.md). A
step that *discovers* downstream work (a fan-out per conversation, say)
does it inside one step rather than adding steps: the graph changes only
when the config does (§"What the loop runs, and what makes a step
stale").

**The runner knows no program by name.** Everything it does with a step
follows from that step's config entry and from facts every step has,
never from which program the step runs. `datalib-step`,
the step program Datalib ships, is one program among others: a Datalib
config spells out in its own entries what the runner should do with it
(its `reads`, its `locks`, the verbs its `[[programs]]` entry lists).

## A step is an id and a command

A `[[steps]]` entry's `id` is the tree the step writes, the key its
state is recorded under, and what another step's `inputs` name. Its
`command` is required: the program the step runs and every argument it
needs, split shell-style. A step's id never changes; renaming it is a
new step, with a new tree.

The id's first `/`-separated part is the step's **group**. A
`[[groups]]` entry with that id is the container a person thinks of as
one thing — "Work Slack" — with an `id`, a `name`, and optionally a
`type` and a `description`. A step whose first part names no
`[[groups]]` entry is a step outside any group. A group is used for
these things only:

- the Manage screen files the group's steps under it, and labels them
  with its `name`;
- `POST /api/requests` opens one request per group its roots belong to,
  so each source's sync has a Stop of its own (§"The record");
- a purge deletes the tree of a group the config no longer names
  (§"Resets and purges");
- a group whose entry is dropped drops every step under it
  (§"Diagnostics: severity is blast radius, not mood").

None of a group's keys is handed to a step or fingerprinted. A rename
re-runs nothing. The `type` is opaque to the runner: it is for whoever
shows the pipeline, and a program that needs it takes it on its command
line, as `datalib-step run ingest slack` does. Changing a `type` alone
re-runs nothing; changing the command that carries it does.

## The graph is declared, not derived

Step A → step B iff B names A's id in its `inputs`. A step's id is also the
one tree it writes, so an input is simultaneously a step reference and an
artifact path, and nothing has to be matched against anything.

One input may name many steps: `*/render_markdown` is every step whose id has
two parts, the second `render_markdown`. The loader expands it to those ids before anything
else sees the step (`src/input_pattern.rs`), so the graph, the fingerprint and
the step's `DATALIB_DAG_INPUTS` hold ids only, and a source added to the file is read
without its readers being edited. A pattern is still written in the config:
the edges stay where a person can read them, and `check` prints what each
pattern stood for.

Validation is correspondingly small: group ids are one segment and
unique, every part of a step id is a usable directory name, step ids
are unique and un-nested, no step writes under `system/` or
`datalib_curated/`, every lock a step holds is declared, every input
names a declared step, no step consumes its own output, no cycles.

**A step that breaks one of those is left out of the graph, not carried in
it with a failed status.** The scheduler's invariant is that every artifact
in the graph has exactly one producer; a step whose input names nothing
would make the runner invent a version for a tree nobody wrote. Excluding it
keeps the invariant without touching the scheduler.

Dropping cascades. A step whose input names a step that was itself
dropped is `Blocked`, not `Rejected` — nothing is wrong with it, and
sending the reader to its line would send them to the wrong line. Graph
assembly is told what the config pass already threw out so its message
can tell "you named a step that does not exist" from "the step you named
is broken"; the commonest case of all is a render step whose fetch step
was rejected for a bad key.

A pattern does not cascade. It stands for the steps that can run, so a step
dropped for its own reasons, or for an input it names by id, falls out of
every pattern that would have named it (`without_broken_matches` in
`src/config.rs`). One broken source then costs its own steps and not the grid
that reads every render by pattern; named by id, it still blocks the step
that names it.

Cycle reporting separates the ring from what merely hangs below it. Kahn's
algorithm cannot: it leaves behind everything it could not order, ring and
tail alike, and telling someone a step three hops below a cycle is *in* it
sends them looking for an `inputs` entry that isn't there.

## What the loop runs, and what makes a step stale

The loop runs what open **requests** want. A request is a row in
`system/supervisor.sqlite` naming its **roots** — the steps a Sync was
pressed on, or every step with no inputs for a `datalib-pipeline sync`
that names no step — and who opened it (`opened_by`). Its **scope** is the roots and everything
downstream of them. Anyone may open one, or ask one to stop, or turn a
step off: that is a row too. Only the process holding `runner-lock` runs the
loop, and it hears of new rows because whoever writes one announces it
(below, "What wakes the loop"). A **run** is one busy period of the loop — from taking a
request on while idle to having none left — and every request served in
it shares that run's id.

Scope is reachability in the graph, deliberately independent of run-time
state. That is what makes "sync yolink" mean the same thing every time — the
set of steps that can move is a property of the config, readable off the
DAG. The cost is that pending work elsewhere stays pending until a request
reaches it, and in exchange a per-source sync never does surprising work on
someone else's chain. A step in scope that reads one outside it reads that
step's recorded version. A step no open request wants is never started,
and a run records nothing for it: its `last_run` stays as it was, so a
sync of Slack never moves email's "last synced".

**Which step does what is one pure function**, `supervisor/tick.rs`: from
the graph, the open requests, the steps turned off and the facts (each sink's
version, what each step last read, what is running), it gives every step
a state, and the starts, stops and request closures to make.
`supervisor/round.rs` is the loop that feeds it and acts on it. The
states, as the record's `steps.state` stores them and a Manage row shows
them:

| state | meaning |
|---|---|
| `running` | an invocation is live — or it ran a pass and what it reads has not settled (below) |
| `waiting` | wanted and due, and held back; `waiting_for` says by what kind of thing, `state_detail` by which |
| `fresh` | wanted, and up to date |
| `off` | someone turned it off; `turned_off_by` says who |
| `blocked` | wanted, but a producer it reads has never published and is not going to run |
| `failed` | its last run failed; for a wanted step, after the request opened and on what it reads and its definition now (rule 4) |
| `idle`, `stale` | no open request wants it; up to date, or not |

The tick is level-triggered: every wake-up, whatever caused it,
recomputes every step from the state as it is, so a burst of wake-ups is
one look and a lost one costs latency, never a wrong start. **A step
starts** in a tick, visited in topological order, iff:

1. **it is not running**: one instance of a step at a time;
2. **it is not turned off**;
3. **an open request wants it**: some request's scope (its roots and
   everything downstream) holds it;
4. **it has not failed for every request that wants it**: its last run
   failed, after that request opened, on the inputs and definition it has
   now. A run the loop asked to stop, and that stopped, is neither a
   failure nor a run: turned on while a request wants it, the step runs
   again. One that says it failed has failed, whatever it was asked;
5. **it is due**: it is **stale** (it has never succeeded, an input's
   version differs from the one it read at its last success, its
   fingerprint — its id, argv, params, env, declared inputs and
   `code_version` — differs from the one recorded then, or it answered
   a start's `--prepare` with `needs_rerun` and has not succeeded
   since), or it declares no inputs and has not run since the
   request opened, since a source's real input is outside the graph. A
   fan-in whose `inputs` are patterns that match no step yet (`*/embed`
   where no source is searched by meaning) reads nothing too, but it
   declared inputs, so it is no source: it runs only when stale;
6. **no producer it reads holds it**: none is running without streaming
   (or with this step reading its files), and none is about to
   run, held only by a lock or a reader, since that one would
   rewrite what this step reads. A producer waiting on its own upstream
   holds nobody back, so a fan-in never waits for its slowest source.
   A step that says `start = "after_inputs_settle"` waits for more:
   while any step upstream of it is running or waiting, streaming or
   not, it waits, so it runs once on what they ended with rather than
   once per seal;
7. **it has something to read**: a step with inputs none of whose
   producers ever published is `blocked`, or waits if one is about to run;
8. **every lock it would take is free** (below, "What keeps steps apart");
9. **the data root's disk has room**: it is not under the config's
   `[disk_space]` floor (below, "When the disk runs low").

A step that waits says why twice in its row. `waiting_for` is the
kind, one word a program can test (`WaitKind` in `supervisor/tick.rs`):
`upstream`, `reader`, `lock`, `disk_space`, or `config` for a step a
request names that the config the loop runs does not have yet.
`state_detail` is the sentence a person reads: `waiting for a`,
`waiting for c, which reads what this writes`, `waiting for lock gpu,
held by trainer`, `waiting for disk space: 3.2 GB free on the data
root's disk; steps pause under 10.0 GB and run again from 15.0 GB`. Each step writes only the tree its id names, so
no two steps ever wait on each other as writers of one sink.

A step that has read every seal so far is between passes: no process is
running, but the step is not finished either, because what it reads is
still being written. The record says it is `waiting` on the first
producer it reads that has not settled (`waiting for garmin/ingest`),
and the Manage row reads Waiting rather than Queued, since it has
already started. A producer that is itself between passes has not
settled, so the index behind a render waits for as long as the download
runs. Each pass's process is closed with a `PassEnd`; the `StepFinish`
comes once its producers are done.

**A failed pass is reported even when the next one succeeds.** A pass
that fails while what it reads is still moving runs again once it moves
(rule 4), and the run then reports the pass that ran last: a consumer
whose first pass crashed and whose second worked has succeeded. So the
`PassEnd` carries the pass's status and error, and the run store keeps
a failed one as an `error` line of the step's log (`pass 1 failed: …`,
with the exit status and the end of its stderr). The loop counts the
passes that failed and were run again, and reports the count as the
step's `failed_passes` metric when the step finishes, zero included, so
each run that invokes the step replaces the last one's: the Manage row
adds it to the step's warnings, and `run_summary` and `datalib-pipeline`'s
report on stdout say it too. A pass that fails and is not run again is
the step's last, and its failure is the step's.

A failed step does not stop its dependents. Whatever it committed and
reported is a version like any other, and a dependent reads it; a fan-in
reads every source that worked. A step whose inputs have *never* been
published — a first download that failed — has nothing to read and is
`blocked`. Failure kinds map to a retry policy; the step only classifies.
Retries re-invoke the step inside one invocation, which is safe because
steps promise idempotency, and once they run out the step is not started
again for that request unless something it reads moves.

**A request closes** when nothing in its scope is running or waiting:
`failed`, naming the first step in topological order that failed for it
or was blocked, or `done`. **A stop** closes it at once as `stopped`, and
a running step no open request wants any more gets SIGINT; it
checkpoints and exits, and until it has, its row's button reads
Stopping. **Turning
a step off** keeps it from starting and stops it if it is running; a
step turned off that a request skipped takes no part and records no run.
A run the loop stopped is not a failure (rule 4); one that reported
`cancelled` on its own is. A step turned off while the loop is idle
reaches the record through `Runner::settle`, one tick with nothing open;
the idle host settles again after every busy period and on a config
change, and compares the switches it finds later with the ones the
settle recorded, not with any it read before. A request naming a step
the loop's config lacks is closed `failed` if it was open when the busy
period began; one that arrives mid-period waits for a config that has
the step.

**The loop re-reads the config while it runs**, when told it changed
(below, "What wakes the loop"). A source added mid-sync
starts beside the sync already going, and a step edited mid-sync runs
under its new definition from its next start. A running step keeps the
definition it started with and records that one, so the edit leaves it
stale and it runs again if a request still wants it. A config that drops
a step still running, or still named by an open request, is taken on once
the loop is done with that step: a config saved mid-edit must not cost a
long download. The step environment (`PATH`, log level, the config's
`[env]`) stays the one the busy period started with.

A reset, a purge and freeing disk space are carried out by the loop too
(§"Resets and purges").

## When the disk runs low

Every config has a floor on the free space of the disk the data root
lives on, with two lines: under the **pause** line the steps are held,
and once held they stay held until the disk is back to the **resume**
line. The gap keeps a disk hovering at one line from stopping and
restarting the same steps over and over. The defaults are 10 GB and
15 GB; a config moves them in a `[disk_space]` table, as bytes or with
a unit (the grammar is `source_common`'s `byte_size`):

```toml
[disk_space]
pause_below_bytes = "10 GB"
resume_at_bytes = "15 GB"   # left out: 5 GB above the pause line
```

A resume line under the pause line refuses the config, and
`pause_below_bytes = 0` turns the floor off. While the steps are held:

- **nothing starts**, and every step that is due reads `waiting for disk
  space` (rule 9) and says so once on its log, the only place a one-shot
  `datalib-pipeline` shows it;
- **everything running is stopped**, as a stop is: SIGINT, then a
  checkpoint and an exit. That is neither a failure nor a run (rule 4),
  so the step runs again once there is room, and each stopped step's log
  says why;
- **the requests stay open**, since their steps are waiting, and the
  sync carries on where it was once space is freed or the floor is
  lowered. A request whose scope has nothing due still closes.

The loop logs the crossing once each way (`WARN` going under the pause
line, `INFO` reaching the resume line). Which side of the gap a disk is
on is remembered for one busy period, so a sync started anew with the
disk between the lines runs. "Free" is what an unprivileged process may
still write (`statvfs`'s `f_bavail`); on APFS that leaves out purgeable
space, so it can read lower than Finder. A disk that cannot be measured
holds nothing back, and says so once at ERROR. An edit to the floor
reaches a loop already running. The app's status bar draws the same
number beside the root's size, raises a toast while the steps are held,
and keeps the samples in `system/disk_stats.sqlite`.

## Upgrading a root

A new build of a program can change the shape of what its steps wrote:
for `datalib-step`, a raw store's tables, a render store's, the grid
index's. The runner knows none of those shapes, nor when a program
changed; each step answers for its own. **Each time the loop starts**,
the process that holds `runner-lock` (the http server or a
`datalib-pipeline serve` when it takes the lock, a `datalib-pipeline` sync when it
runs on its own) asks every step whose
program's `[[programs]]` entry lists the `prepare` verb, producers
first, before the loop takes any request: `Runner::prepare` invokes
each once with `--prepare` appended
(`docs/dev/step_protocol.md` § Prepare). Since it asks at every start,
a step answers quickly when there is nothing to do. It answers in one
of four ways:

1. **Nothing to do.** The record is left as it was.
2. **Brought up to date in place.** The step reports its output's new
   version, which the record takes while keeping the step's last
   success, so what reads the output is stale. A `datalib-step` raw
   store is migrated this way.
3. **`needs_rerun`.** What the step wrote can only be rebuilt by running
   it. The record keeps the flag (`steps.needs_rerun`), the tick holds
   the step due until it next succeeds (rule 5), and
   `round::rerun_offer` lists it for the app, which asks whether to run
   it now and opens one request rooted at the list if so. A sync that
   does not reach such a step leaves it alone. A `datalib-step` render
   store or the grid index in another shape answers this way, and so
   does `unified_grid_and_search_index` when a source it reads was drawn
   into the search index by another version of the index text or its
   chunking (`INDEX_TEXT_VERSION`), which no render moved.
4. **Refused** (`failure: incompatible`). What the step wrote is in a
   shape its program cannot use, such as a newer build's. The runner
   turns the step off, as turned off by `the runner, because the step
   refused --prepare` (`scheduler::REFUSED_PREPARE`), and a later
   start whose `--prepare` does not refuse turns it back on. A step a
   person turned off keeps its switch.

A step that fails to answer costs that step only, and the rest are
asked. For `datalib-step`'s own steps: the grid index reads a render
store in another shape as the index had it, with a warning; a rebuild of
the index has nothing to keep, so a source whose render step stays
turned off then has no rows until it re-renders. Its documents stay in
the search index with their vectors, and a search reads only the
sources the grid holds, so it leaves search too and costs a search
nothing. In the app, the pass writes only what each step brings up to
date in place, so the app stays usable while it runs: a pill where the
sync indicator sits says how each step is going, and every Sync on the
Sources card is disabled (`ManageResponse::sync_paused`); a request
opened another way waits for the pass. After it, the pill stays red for
a step the pass could not prepare until it is dismissed. The pass is
held in the server's memory (`/api/config`'s `upgrade`) and announced as
`pass_changed` on `/api/sync/stream`; freeing disk space is drawn by the
same pill (§"Resets and purges").

Before any of that, the http server moves each tree a config rewrite
renamed to its new path, with the record's rows for its steps
(`http/src/config_upgrade.rs`): the index under an earlier group name,
the index step's `grid_index/` tree, and a folder an older config
spelled with capitals.
[`config_model.md`](../../../docs/dev/config_model.md#the-retired-shapes)
has each rewrite and its move. In the repo,
`datalib/backend/datalib_step/raw_shapes/` holds the raw shape every
release left, and a test migrates each one to this build.

## What keeps steps apart: locks

What keeps steps apart is locks, and every one is in the config or
follows from it:

- **A sink is a read/write lock.** Its writer holds `write`. A step that
  reads it off disk (`reads = "files"`, the default) holds `read`: no
  writer of what it reads runs beside it, in either order. A step that
  says `reads = "pinned"` holds nothing: it promises to read only the
  commit `DATALIB_READS` names, and the writer may go on writing.
  Whether a consumer may *start* on a producer that is still running is
  not a lock but rule 6: only on a producer that declares
  `streams_output`, so a seal it reads is a whole one.
- **A named lock** is a `[[locks]]` entry: a `name` and `slots` (default
  1, a mutex). A step names what it holds: `locks = ["quota"]` takes one
  slot, `locks = { gpu = "exclusive" }` takes them all. For what the
  graph does not show: two sources on one account's rate limit, a GPU,
  several steps writing one file. A step that holds a lock no
  `[[locks]]` entry declares is dropped.
- **Every config has two default locks** (`supervisor/locks.rs`):
  `network` (4 slots) and `cpu` (4). A step that names no locks holds
  one of them: `network` for a step that declares no inputs, `cpu` for
  any other, a fan-in whose patterns match nothing yet among them, so a
  download waiting out a rate limit never keeps a render from starting.
  A `[[locks]]` entry of the same name resizes one, and `--parallelism
  N` sets both to N, over the config.

A Datalib config declares two more, and its `datalib-step` entries name
them:

| lock | slots | held by | why |
|---|---|---|---|
| `embedder` | 1 | every `embed` | each loads the embedding model and writes the search index's inference cache, which the runner cannot see as shared |
| `index` | 2 | the index steps (`unified_grid_and_search_index`, `time_series_index`, `embedding_map`) | a budget of its own, apart from the renders' `cpu` |

The index step, `unified_grid_and_search_index`, needs no lock of its
own: it is the one writer of the grid index's file, and an `embed` reads
that file off disk, so the two never run together.

None of `locks`, `reads` and `start` is in the fingerprint: they change
when a step may run, not what it makes. A config edit that changes only
them is still taken on mid-sync, like any other. A Datalib config
writes `reads = "files"` on the `datalib-step` steps that read their
inputs off disk (`embed`, which reads the search index as it is;
`embedding_map`, which reads the search index and its inference cache;
and perseus's render, which reads its TEI files) and `reads = "pinned"`
on the rest that have inputs.

## What a sink owes its consumers

A step's sink is whatever it writes: a doltlite store, a plain SQLite
file such as the grid index, a directory of files. A consumer relies on
two separate properties, and conflating them produces a contract that is
false for half the sinks.

**P1. "Absent" is not "empty."** A sink never reports "there is nothing
here" when it means "I could not read this." Every sink owes this,
streaming or not, because a consumer that reads an empty sink concludes
the source holds nothing — and render then deletes every document it
had. For a doltlite store the line is the *schema commit*: a store that
has one is readable, possibly empty, and zero rows means zero rows; a
store with no file, only doltlite's "Initialize data repository" commit,
or tables and no committed schema is unreadable, `pin::head` refuses
it, and the consumer skips without sweeping.

**P2. Readable while it is being written.** A consumer can take a
stable view of a producer still writing only if the sink's engine gives
one. A doltlite store does: a reader pins a commit, and the producer
seals commits beside it
([`docs/dev/doltlite.md`](../../../docs/dev/doltlite.md#three-ways-to-read-one-commit)).
A plain SQLite file rewritten in place, a directory being emitted and
an appended file do not. So P2 is the step's to declare — a `capabilities` event
with `streams_output` — and its default is no: an undeclared output
keeps its edge a barrier (rule 6 above;
`a_producer_that_does_not_declare_streams_output_dispatches_nobody_early`).
The step declares it, not the config, because only the sink's author
knows, and a wrong "yes" is a torn read nobody sees.

A producer that streams seals as it goes: it commits at a consistent
point and says so with a `checkpoint` event
([`step_protocol.md`](../../../docs/dev/step_protocol.md)), and
`datalib_etl::checkpointer` decides when.

## How the loop is proven

`//datalib/backend/dag:supervisor_harness_test` asks one question: does
the loop manage processes correctly? A puppet step (`tests/puppet`)
does only what it is told over a FIFO and acks each instruction; the
harness (`tests/supervisor_harness`) writes a real `config.toml` of
puppets, runs the loop in-process under `host::run_idle` as `datalib-pipeline
serve` does, and plays a person through the store. It waits only on what it
can observe (an ack, a loop event, an announcement), each under a
deadline, and asserts only what the loop owns: processes started and
ended, never two of one step at once (checked on every start), each
request's outcome, each run's recorded status, the versions recorded
and handed on, the queues. Nothing about what a step wrote. Product
timers a scenario depends on (stop grace, retry backoff, backstop) are
parameters it sets; no test sleeps.

Beside the scenarios, a seeded random walk over all of it keeps the
invariants after every episode. A failure prints its seed and the last
40 things seen; `HARNESS_SEED=<n>` replays one walk,
`HARNESS_SEEDS=<n>` runs that many (32 by default). From a checkout:

```sh
bazelisk test //datalib/backend/dag:supervisor_harness_test --test_env=HARNESS_SEED=27
```

## Versions: reported by the step

**The loop never opens a step's output.** It takes the version a step
reports, on each seal and in its outcome, and compares it for equality,
nothing more. That is what keeps it agnostic to what the steps run: a
doltlite store, a directory of files, or anything else.

**A version should be a function of the output's content**, so that two
runs over the same data report the same string and "unchanged" is
something the loop *derives* rather than something a step asserts. A
doltlite store has two: its head commit, which moves only when a commit
changed something ([`doltlite.md`](../../../docs/dev/doltlite.md#diffs)),
and the hash of that commit's tree, which also stays put when the
store's history is folded into fewer commits
([Trees](../../../docs/dev/doltlite.md#trees-and-keeping-one-through-a-fold)).
`render_markdown` reports the tree, since nothing reads its history;
`unified_grid_and_search_index`, whose index is plain SQLite, reports its generation, a
count it raises only when a reader would see a change; the time-series
steps report the head commit, since the time-series index reads each
store at the commit it recorded. An ingest reports a commit too, so a
consumer can pin a read to it, but not its head: every sync commits its
own record and stamps, so the head moves even when nothing upstream
changed. It reports the last commit at which a table its readers read
moved, rows or shape (`doltlite_raw::moves_version`: every content table,
`problems`, `ingested_files`; never a `*_bookkeeping` sidecar,
`sync_runs`, the cursors, `coverage` or `coverage_checked`). The writer
keeps that commit on
a branch, `datalib_content`, moved at each seal
(`doltlite_raw::CONTENT_BRANCH`). Each spells its version the same
way on a seal and in the outcome, so finishing on the commit last
sealed moves nothing downstream.

**A step that reports no version gets a new one on every success**
(`<fingerprint>:run-<invocation id>`), so everything that reads it runs
again, as `make` would with no timestamps to compare. That is always safe
and sometimes wasteful; the loop says so on the event stream when it
happens, and a step that wants its readers to skip reports a version.

**A failed or stopped step's reported version stands**: what it
committed before it ended, its consumers read. `datalib-step` reports
its store's version however it ends. One that reports nothing moves
nothing, since its tree may be half-written. A step killed outright
after publishing and before saying so reaches its consumers at its next
run, not at once.

A step that did not run contributes the version recorded for its output
last time, or `UNKNOWN`, which compares equal to itself so two runs that
both know nothing agree.

**The step's fingerprint is folded into every version.** A step
reports on its content and cannot know its own definition changed;
without folding, a bumped `code_version` would re-run the step while its
version stayed the same, so its consumers would skip and keep what the
old definition produced. A real version therefore always contains a
colon (`<fingerprint>:<version>`) and never equals `UNKNOWN`.

## Diagnostics: severity is blast radius, not mood

The loader returns a list of diagnostics rather than an `Err`, so that one
stray key in one step cannot take down the grid, search, the document view
and every applet — the applets are declared in the same file.

The four severities say what a problem *costs*:

| severity | meaning | cost |
|---|---|---|
| `Fatal` | the file is not a config | nothing runs; the only one that blocks the app |
| `Rejected` | this entry is unusable | that entry is dropped, every other loads |
| `Blocked` | this entry is fine and cannot run anyway | dropped too, but the fix is elsewhere in the file |
| `Warning` | valid, probably not what was meant | nothing is dropped |

`Rejected` and `Blocked` have the same consequence for the scheduler and
deliberately different consequences for what the reader is told. Merging
them would be the cheaper code and the worse error message.

A group whose id is bad costs the group *and* every step under it, and
those steps are `Blocked`, not `Rejected`: nothing is wrong with them,
and the fix is on the group's line. The two warnings: a group nothing
is filed under, and an applet filed under a group that does not exist.
The Manage screen's System row counts the warnings and shows their
words on hover. A config in the shape from before the runner stopped
knowing `datalib-step` is refused entry by entry, each refusal naming
the rewrite (`config::RETIRED_SHAPE_HELP`): a step that writes `group`
or `function`, or has no `command`, is `Rejected`, and so is a
`[[groups]]` entry with `source`; a top-level `checkpoint_cadence`,
`render_pages`, `[modal]`, `embed_contexts` or `embed_search_index` is
`Rejected` and the steps still load. The app rewrites such a config when it next reads it, and
`datalib-migrate-config` does from a terminal
([`config_model.md`](../../../docs/dev/config_model.md) §"The retired
shapes").
The full list of what the loader drops is in
[`config_model.md`](../../../docs/dev/config_model.md) § "What the loader checks". A warning passes the strict door too
(`config::parse`, and the `PUT /api/config` behind the editor): it
changes nothing about what runs, and refusing it would make the editor
unable to save a config the app is happily running on.

Every located diagnostic carries a **byte span** into the config text, not a
line number: the terminal wants `file:line:col` plus an excerpt and the UI
editor wants a selection range. Deriving a span from a line loses the column
and the length; deriving line and column from a span is exact. Diagnostics
raised during graph assembly know a step id but not where it sits in the
file, so the loader — which holds both — lends them the location rather than
threading the config text through graph assembly.

The excerpt is drawn by us rather than taken from the TOML parser's own
rendering, so a diagnostic the parser never saw (a duplicate id, an input
naming no step) looks exactly like one it did.

## Two locks, two files

- **One loop per data root** (`system/runner-lock`). The loop is the
  only writer of its record, and the steps it spawns write raw stores
  whose doltlite working set is shared across every connection on the
  branch, in any process ([`doltlite.md`](../../../docs/dev/doltlite.md#branches-head-and-the-working-set)); two loops on one root would interleave both. While `datalib-http` is up it keeps
  `datalib-pipeline serve` (`supervisor/serve.rs`) running as a child, which
  holds this lock and runs the loop, so every other
  `datalib-pipeline` sync is a client of it. A `datalib-pipeline` is never refused for the lock: a sync is a request
  row in `system/supervisor.sqlite`, so it writes its row and follows it
  while whoever holds the lock runs it, trying the lock again whenever
  anything is announced, its release included, in case that loop ends
  first. Only `reset`, which empties stores, and `free-disk-space`,
  which rewrites them, need the root to themselves and is refused while a loop runs —
  always, with the app up; the app's resets are rows whichever loop
  holds the lock carries out (§"Resets and purges").
- **One server per data root** (`system/lock`), which `datalib-http`
  takes for its own reasons (the API token, the feedback, usage and
  remote-media stores).

They must be *different* files: a server that starts while a
`datalib-pipeline` runs the loop holds the root as a server, and its
child waits for `runner-lock` — the UI's syncs, resets and purges are
rows that loop serves meanwhile — and takes the loop over when it ends.

The server restarts a child that dies, after one second, doubling to a
minute, each time logged at ERROR; after three deaths in a row, each
quicker than a minute, every Sync button says the runner keeps stopping,
until a child holds the lock again. With no `datalib-pipeline` to start
(`$DATALIB_PIPELINE_BIN`, `$DATALIB_BINARY_DIR`, beside the server, then
`PATH`), the server serves everything else and every Sync button says
why. The child is handed the server's environment but the server's own
token and address and the protected-library flag
(`datalib_env::inherit_for_runner`): it is the runner that hands each
step only what the step protocol allows. At shutdown the server sends it
SIGTERM; one that outlives the server's wait sees its parent pipe
close, and stops its steps the same way.

Whoever runs the loop owns its steps' processes. Each step holds a pipe
from that process (`DATALIB_PARENT_PIPE`) and stops itself when the pipe
closes, however the process died. A step the loop stops gets SIGINT on
its process group and SIGKILL on it fifteen seconds later if it, or
anything it left in the group, is still there (`subprocess::stop_ladder`,
`step::STOP_GRACE`), so one that ignores its SIGINT cannot hold its
store for good, and a child it left behind cannot hold the run open. A loop that died
holding the lock leaves its run and its invocations open in the record
and the run store; the next process to take the lock closes them
(`supervisor::host::take_over`). Its requests are still open rows, and
the next loop runs them.

`flock(2)` rather than a pid file, because the kernel releases it when the
holder dies — a crashed process leaves no stale lock to reason about. The
file's contents are advisory: they exist so a refusal can name the holder,
and are never trusted to decide whether it is held.

`FileLock::is_held` (`datalib_flock`) is a **read-only** probe, separate
from `FileLock::acquire`, which creates the file and rewrites its
contents: a caller asking on a timer must not rewrite the file, and a
root that never ran must not sprout one. It is racy by nature (the
holder may let go a microsecond later), so build no invariant on it.
Nor is it invisible: `flock(2)` cannot ask without taking, so the probe
holds the lock for an instant. A `reset` that finds the lock held
therefore keeps trying for two seconds before it refuses, and a probe
that found the lock free announces that it let go, so a sync that met
the probe takes the lock at once.

## Progress: the store takes positions, never deltas

The run store (`system/runs/runs.sqlite`, written through `runs_sink.rs`)
coalesces — of the ticks between two flushes only the newest is written —
and coalescing deltas silently loses work, turning "347 of 900" into
whatever fraction of the increments happened to land on a flush boundary.

So `Event::Metric` carries an **absolute value**, and the one event that
still carries a delta, `Event::ProgressInc`, is summed per step in
`runs_sink.rs` before anything reaches the store. Dropping a position is
lossless, which is what makes the coalescing correct rather than merely
cheap.

### One bar per step, and why a download must use `RunBar`

From those two events the sink derives the pair a reader actually wants:
`done`, the increments so far, and `queued`, what the announced total
leaves. `queued` is the Manage screen's Queue, and `done` (the series
`done_total`) the pace its
ETA reads. The scheduler keeps a pair of its own per producer for each
consumer (`QueueLedger`): `queued{from=…}`, the rows sealed and not
yet read, which climbs seal by seal and falls to nothing when a pass
ends, and `dequeued_total{from=…}`, the rows taken off so far, which only
grows — the pace a sampled sawtooth can lose the bottoms of. When a
step ends, however it ended — succeeded, failed, stopped, turned off —
the sink sets every `queued` series of that step to zero, its own and
each `queued{from=…}`: a step that is not running has nothing ahead of
it in this run.

When the step ended short of done (stopped, turned off, failed, or
`incomplete`), what its own `queued` held at that moment is what it
still owes: the conversations, threads and files a Slack sync saw
waiting and did not get to. The sink writes it first as the step's
`owed` (`datalib_metrics::OWED`), which nothing zeroes. A step that
succeeded owes nothing, whatever its bar still showed: a render or an
embed often ends a successful run with work announced that it then had
no need to do, and a download that cannot finish says so by ending
`incomplete`. So every step a run invokes gets an `owed` as it ends,
zero when it succeeded or never counted, and the next run that invokes
it replaces it (`runs_sink::owed_at_end`). A `queued{from=…}` is not
owed: those are the producer's seals the step has not read, which its
next run reads anyway. The Manage row of a step at rest draws it ("412
owed"), and `datalib-pipeline status` lists it. A record the step reached and
failed is usually counted done, so the count can fall short of what the
store owes by the records that failed, which are errors on the row.

The trap is that `done` accumulates across **everything** the step
reported, while `total` is simply whichever length it announced last —
and the runner relabels every event to the step that emitted it, so a
step cannot have two independent bars even if its code looks like it
does. Two bars each announcing their own size therefore pin `queued` at
zero from the second one onward: `done` already carries the first bar's
work, and the subtraction saturates.

So a download reports through one `datalib_etl::progress::RunBar` for
the whole run, whose total only grows — each phase adding what it
has learned it will do — but for work the run takes back
(`RunBar::forgo`): what a cap in the config leaves out, what upstream
refuses to show, what the store turned out to hold already. What is
still on the bar when the step ends is read as owed, so a tick
announced has to be done, owed or taken back. Announcing no total at
all is fine and means "size unknown"; the sink then publishes no
`queued`, which is not the same as publishing zero, because zero means
finished.

**Count only work the run can see waiting.** Something announced in the
same breath as it is done — a page's messages, when upstream names no
count ahead — adds to the pace and never to the queue, so the ETA,
which is one over the other, reads seconds while the real work is
hours. Slack counts conversations, threads and files for that reason,
and leaves messages to its status line.

## What wakes the loop

Nothing on a timer. **Whoever commits to `system/supervisor.sqlite`
announces it**, the way a step announces a seal. `Store` does, after
every commit it makes (`request opened <id>`, `turned off <step>`, `record
saved`, …), and so do the two writers that are not the store: the
server, when its watch sees `config.toml` move (`config changed`, which
is what makes the loop re-read its config), and whoever lets go of
`runner-lock`. `supervisor/announce.rs` is all of it.

A listener is a FIFO in `system/supervisor-listeners/`, named
`<pid>-<n>.fifo`. An announcement is one line, shorter than a pipe
writes at once, written to every FIFO there. A FIFO nobody holds open
belongs to a process that is gone, and the next announcement removes it.
A listener is made before the state it guards is read, so a line
announced in between waits in the pipe. The loop, the idle host, the CLI
following its request, `POST /api/requests` waiting for the loop to take
its request on, and the UI's change stream each hold one.

**Why not watch the database file.** A file watch fires on writes, not
commits: in rollback-journal mode SQLite writes the database file during
a commit, and a large transaction can spill pages before it commits.
FSEvents says nothing about writes to a file a process holds open, which
the loop always does. And it needs code per OS. Why FIFOs and not Unix
sockets: a socket's path is limited to about 104 bytes, and a Bazel
test's temporary directory is longer than that.

**The backstop.** A listener that has heard nothing for 30 seconds reads
the store's `PRAGMA data_version`. If the store moved and nothing is
announced for it by the next look, a writer bypassed `Store` (a `sqlite3`
shell, an older build): that is logged at ERROR and counted
(`announce::missed_announcements`), and tests assert the count is zero.
It is the only timer that wakes an idle loop, and it exists to catch
that bug. The other, while a round is open, is a look at the disk every
five seconds for the `[disk_space]` floor: nothing announces a disk
filling or being freed.

A `datalib-pipeline` running the loop with no server up, a sync or a
`serve`, has nobody watching the config, so it takes an edit on at its
next busy period, not mid-sync.

The loop's idle side lives once, in `supervisor::host::run_idle`: a busy
period whenever a request or a wipe is open, requests asked to stop
before any period took them closed as `stopped`, the record settled when
the switches or the config move, then a wait for an announcement or the
host's stop. `supervisor::serve::run` runs it after taking the lock and
the prepare pass; `datalib-pipeline serve` calls that, and
the harness runs `run_idle` itself. A `datalib-pipeline` sync has no idle
side: it serves until nothing is open, then exits.

`datalib-pipeline serve` says on stderr, one NDJSON line each
(`supervisor::serve::ServeEvent`), what its parent needs and no store
holds: that it holds the lock (`holding`), the prepare pass
(`prepare_plan`, `prepare_step`, `prepare_done`), each busy period
(`sync_started`, `sync_over`), and every write it makes to the run
store (`run_store_written`), which a filesystem watch would report late.
Its own log lines go to the run store, as a `dag` process outside any
run, which the log card offers under "The runner". SIGINT or SIGTERM
stops it: its steps are interrupted and their requests stay open. A root
with no config, or one it cannot read, is served all the same, and
nothing runs until the file holds a config.

## Resets and purges

A **wipe** is a row in the mailbox's `wipes` table: a reset (empty what
these steps wrote), a purge (delete the trees of groups the config no
longer names, and forget their steps ran), a compaction (fold away the
history nothing reads and collect the rest) or a collection (delete only
what no commit reaches; [`step_protocol.md`](../../../docs/dev/step_protocol.md#compact-and-collect)). The loop carries one out as
soon as the steps it touches have stopped, whatever else is syncing; it
does not wait for the sync to end. What it decides is in
`supervisor/wipe.rs`, and the loop does it (`round.rs`,
`carry_out_wipes`):

- **It holds its steps.** A step being wiped is treated as turned off for
  as long as the wipe is open: stopped if it runs, not started, and its
  row says `being reset`, `being deleted`, `being compacted` or
  `collecting unused space`. A compaction or a collection holds the steps
  it asks and every step that reads one, since a reader on a commit the
  fold drops would be left reading nothing, and either rewrites the file
  under a reader; with no targets it asks every step whose program
  takes the verb (`StepSpec::takes`). It leaves the requests that
  named it as a root, so a reset does not refill a download, and a purged
  group's config change is not waited on; a request left with nothing is
  stopped.
- **It says how far it has got.** A compaction or a collection writes
  every step it asks into the record's `wipe_steps` table, waiting, then
  each as it runs and ends: done with the store's size before and after
  (its outcome's `store_bytes`), or failed with why. Each write is
  announced, which the server turns into `pass_changed`, and
  `/api/config`'s `free_space` is the newest one since the server
  started. The toolbar draws it with the prepare pass's pill: amber with
  "Freeing space · 7 of 20 · 5.2 GB freed" while it runs, then green
  with what it freed (red with a source it could not shrink) until
  dismissed. Its run is a `shrink`, its reason who asked.
- **Then it does it, in the loop.** A reset invokes each step whose
  program takes the verb with `--reset`, deletes the tree of any other,
  and records it emptied in the loop's own record, then
  opens a request for what reads it (`wipe::after_reset`). A purge waits
  until the loop has taken on the config without the group, then deletes
  `<root>/<group>/` and forgets the group's steps.
- **Refused, with why**, against the config on disk: a reset of a step it
  lacks, a purge of a group it still has or of anything that is not a
  group id. The row closes with the reason in `error`.

The server's `POST /api/reset`, `POST /api/purge` and `POST
/api/free_disk_space` (a compaction with
`drop_old_versions_on_derived_data`, else a collection; `POST
/api/compact` is the old spelling of the first) write the row and wait
up to ten seconds for it to close: 204 once done, 202 when its steps
have not stopped yet or it is still going, 409 with the reason when refused. A compaction or a
collection opens nothing downstream: each keeps every store's content, so
no version moves.

From the command line, `datalib-pipeline free-disk-space` (with
`--drop-old-versions-on-derived-data`, a compaction) takes `runner-lock`
itself, as `reset` does, so it is refused while the app or another `datalib-pipeline` runs the loop. `Runner::shrink` asks the same steps and
records the same versions; the binary measures each step's tree as a
directory before and after, never its store, and prints what it gave
back.

## The record

The loop's memory is its **record**, in `system/supervisor.sqlite`
(`supervisor/record.rs`), beside the mailbox anyone writes
(`supervisor/store.rs`): `requests` (its `roots`, `opened_by`, a
`stop_requested_by`, and once closed its `outcome` — `done`, `failed`
or `stopped` — and `failed_step`), `turned_off` (`step`,
`turned_off_by`), `wipes` (`kind`, `targets`, `opened_by`, and once
closed the `error` that stopped it, if any) and `wipe_steps` (a
compaction's or collection's steps in order, each `state`, `error` and
`before_bytes`/`after_bytes`). It is plain SQLite in
rollback-journal mode, so any `sqlite3` reads it, and only the holder of
`runner-lock` writes it. `supervisor_contention_test` runs seven
processes on one store (people opening requests, the loop saving, the
server reading) and checks that every write lands and that the file's
header says rollback-journal:

| table | one row per | what it holds |
|---|---|---|
| `steps` | step | its state now (`state`, `waiting_for`, `state_detail`, `turned_off_by`, and `requests`, the open requests it still has work in: a JSON array, NULL once it has none, which is when its Manage row offers Sync again), what it read at its last success (`reads`), under which definition (`fingerprint`), and what happened the last time a run reached it (`last_*`) |
| `sinks` | tree a step writes | the version it last published |
| `runs` | busy period of the loop | when it started and finished |
| `run_steps` | step the newest run has reached | what it is doing in that run |
| `invocations` | process the loop started | when, in which run, and how it ended (`outcome` is NULL while it runs) |

A run must record a state for *every* step in scope (including ones that
were skipped or blocked and never "ran"), a `finished_at_utc` that tells
a completed run from a crashed one, and per-step timings. The loop holds
the record in memory (`record::Record`) and saves only what changed since
its last save (`record::changes`), after every tick and every event, not
only on terminal states; a Manage row's Status is `steps.state`, read
straight from it.

The run id is `DATALIB_DAG_RUN_ID`, verbatim — a UUID v7 the host mints
for one busy period of the loop (`datalib-pipeline` takes `--run-id` instead
when given one). `supervisor::host::step_env` puts it in the
child environment and `start_record` hands the same string to the run
store (`system/runs/runs.sqlite`) *before* the loop starts, and `Runner`
reads it back out of that environment, so the record, the store and
every step name one run. If
they diverge nothing errors — the store describes a run nobody is
displaying, `/api/dag` reports no progress on the id mismatch, and the
UI silently shows none. `started_at` stays the pinned
`DATALIB_DAG_NOW`, and `finished_at` is read off the same clock
(`datalib_time::RunClock`: the start moved on by the time since), in
the record and in the run store alike, so a run pinned to 2369 ends in
2369. The runner's stamps for a step and its log lines stay on the
wall clock.

The record is the only channel to a reader who did not spawn the run, and the loop
saves it before it closes a request, whether its work is done or it was
stopped, so a reader that sees a request closed never finds a step still
serving it. A step stopped with its request reads Stopping until its
process has exited. `POST /api/requests`
opens one request per group its roots belong to, so each source's sync
has a Stop of its own, and answers only once a step names each new
request (`Store::taken_on`), so the rows read after a Sync already show
them.
