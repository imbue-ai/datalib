# `datalib-dag` — the runner

Reads a `config.toml`, builds a DAG from it, and runs the steps. This file
holds the rules you cannot recover by reading the code; the contract a
step author needs is
[`docs/dev/step_protocol.md`](../../../docs/dev/step_protocol.md). The
node set is fixed before a run starts, so a step that *discovers*
downstream work (a fan-out per conversation, say) does it inside one
node rather than expanding the graph.

## A step is (group, function); its id is composed

A `[[groups]]` entry is the container a person thinks of as one thing —
"Work Slack" — with an `id`, a `name` and, for a source, a `type`. A
`[[steps]]` entry names the group it belongs to and the function it
performs there, and the loader composes its id as `<group>/<function>`.
Nothing writes that id and nothing downstream splits it: it is the tree
the step writes, the key its state is recorded under, and what another
step's `inputs` name. Both halves are permanent — a step never changes
group and a fetch never becomes a render — so the composed id is stable
by construction. The group's `name` is the half that is free to change:
it is never forwarded to a step and never fingerprinted, so a rename
re-runs nothing. Its `type` is forwarded and fingerprinted, so changing
it re-runs every step under the group.

A group's `description` — what the source is to its owner, free text —
is treated exactly like its `name`: never forwarded, never fingerprinted.
Only the source wizard reads it, to edit it.

A step outside any group is a custom executable and writes its `id`
verbatim. That is the only place a step id is written.

Every step under a group gets the two halves and the type in its
environment — `DATALIB_DAG_GROUP`, `DATALIB_DAG_FUNCTION`,
`DATALIB_DAG_GROUP_TYPE` — beside the composed `DATALIB_DAG_STEP`. A
step with no `command` runs `datalib-step`, which dispatches on that
environment and writes the tree its id names; that is why a built-in
step's function is the directory it writes (`ingest`, `render_markdown`,
`grid_index`, `qmd_aggregator`, …), and why the loader requires such a step to
be under a group. The runner never interprets the function itself.

## The graph is declared, not derived

Step A → step B iff B names A's id in its `inputs`. A step's id is also the
one tree it writes, so an input is simultaneously a step reference and an
artifact path, and nothing has to be matched against anything.

Validation is correspondingly small: group ids are one segment and
unique, a grouped step's group exists and its function is one segment,
composed and verbatim ids are unique and un-nested, every input names a
declared step, no step consumes its own output, no cycles.

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

Cycle reporting separates the ring from what merely hangs below it. Kahn's
algorithm cannot: it leaves behind everything it could not order, ring and
tail alike, and telling someone a step three hops below a cycle is *in* it
sends them looking for an `inputs` entry that isn't there.

## What the loop runs, and what makes a step stale

The loop runs what open **requests** want. A request is a row in
`system/supervisor.sqlite` naming its **roots** — the steps a Sync was
pressed on, or every step with no inputs for `datalib-dag` with no
`--sync` — and who opened it (`opened_by`). Its **scope** is the roots and everything
downstream of them, plus any writer of a store in an old shape that a step
in it reads (below). Anyone may open one, or ask one to stop, or turn a
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
`--sync slack` never moves email's "last synced".

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
| `waiting` | wanted and due, and held back; `state_detail` says by what |
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
3. **an open request wants it**: some request's scope (its roots,
   everything downstream, and the writers of old-shape stores those
   read) holds it;
4. **it has not failed for every request that wants it**: its last run
   failed, after that request opened, on the inputs and definition it has
   now. A run the loop asked to stop, and that stopped, is neither a
   failure nor a run: turned on while a request wants it, the step runs
   again. One that says it failed has failed, whatever it was asked;
5. **it is due**: it is **stale** (it has never succeeded, an input's
   version differs from the one it read at its last success, or its
   fingerprint — its id, group type, argv, params, env, declared inputs,
   `code_version` and, for a built-in step, the shape of the store it
   writes — differs from the one recorded then), or it declares no inputs and has not run since the
   request opened, since a source's real input is outside the graph;
6. **no producer it reads holds it**: none is running without streaming
   (or with this step reading its files, or writing a store in an old
   shape, below), and none is about to
   run, held only by a lock or a reader, since that one would
   rewrite what this step reads. A producer waiting on its own upstream
   holds nobody back, so a fan-in never waits for its slowest source;
7. **it has something to read**: a step with inputs none of whose
   producers ever published is `blocked`, or waits if one is about to run;
8. **every lock it would take is free** (below, "What keeps steps apart").

The store shape is in the fingerprint because a derived store takes a
new shape only when its writer runs. Without it, a build that adds a
`grid_rows` column leaves every source with nothing new upstream holding
a render store in the old shape, and the grid index cannot read it.
`BUILTIN_STORE_SHAPES` in `src/config.rs` names the shape of each
built-in function's store; a test in `datalib_step` keeps it equal to the
real DDL.

Being stale is not enough on its own, because a step runs only when a
request reaches it (rule 3): a sync of one source would run the grid
index over every other source's render store in whatever shape the
build before left it. So **a store in an old shape is pulled into the
scope of any request that reaches a step reading it.** A store is in an
old shape when its writer last succeeded under another definition and
the record (`steps.store_shape`) says that definition wrote another
shape, or says nothing. Only the writer is pulled in, not its own inputs
or its other readers, and a reader that is turned off pulls nothing.
Rule 6 then holds the reader until that writer has finished, streaming
or not, since what it has sealed so far may still be in the old shape.
A writer stale for any other reason — its download moved, a params edit
that kept the shape — still waits for a request of its own. When the
reader runs over an old store anyway (its writer failed, or is turned
off), the reader is the backstop: `grid_index` compares a render store's
`_datalib_meta.schema_hash` with the shape it reads, and leaves a store
in another shape as the index had it, with a warning.

A step that waits says why, in its row's `state_detail`: `waiting for
a`, `waiting for c, which reads what this writes`, `waiting for lock
gpu, held by trainer`. Each step writes only the tree its id names, so
no two steps ever wait on each other as writers of one sink.

Until everything a step reads has settled, its row reads Running between
passes: the step is not finished, it is waiting for the next seal. A
producer that is itself between passes has not settled, so the index
behind a render reads Running for as long as the download does. Each
pass's process is closed with a `PassEnd`; the `StepFinish` comes once
its producers are done.

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
long download. The step environment (`PATH`, log level, checkpoint
cadence) stays the one the busy period started with.

**A reset** (`datalib-dag --reset`, the app's Reset, `POST /api/reset`)
empties what a step wrote and records the tree's new version, forgetting
that the step ever succeeded. The app then opens a request: a reset
step that reads something is rebuilt at once, and a reset download is
not refilled — what reads it runs instead, so its documents leave the
grid, and its next Sync downloads everything again. The design is
[`plans/supervisor.md`](../../../docs/dev/plans/supervisor.md).

## What keeps steps apart: locks

What keeps steps apart is locks, and every one is in the config or
follows from it:

- **A sink is a read/write lock.** Its writer holds `write`. A step that
  reads it off disk (`reads = "files"`) holds `read`: no writer of what
  it reads runs beside it, in either order. A step that reads at a pinned
  commit (the default) holds nothing: it reads a snapshot, and the writer
  may go on writing. Whether a consumer may *start* on a producer that is
  still running is not a lock but rule 6: only on a producer that
  declares `streams_output`, so a seal it reads is a whole one.
- **A named lock** is a `[[locks]]` entry: a `name` and `slots` (default
  1, a mutex). A step names what it holds: `locks = ["quota"]` takes one
  slot, `locks = { gpu = "exclusive" }` takes them all. For what the
  graph does not show: two sources on one account's rate limit, a GPU.
- **Every config has five default locks** (`supervisor/locks.rs`). The
  three budgets are `network` (4 slots), `cpu` (4) and `index` (2). A
  step that names no locks holds one of them: `network` for a step with
  no inputs, `cpu` for a step with inputs under a group with a `type`,
  `index` for any other step with inputs; so a download waiting out a
  rate limit never keeps a render from starting. The other two,
  `qmd_keyword` and `qmd_embed` (1 slot each), are held by the built-in
  qmd steps that name no locks: `keyword_index` and `qmd_aggregator`
  take `qmd_keyword`, `embed` takes `qmd_embed`, because they all write
  one qmd index file the runner cannot see as shared. A `[[locks]]`
  entry of the same name resizes one, and `--parallelism N` sets
  `network` and `cpu` to N, over the config.

Neither `locks` nor `reads` is in the fingerprint: they change when a
step may run, not what it makes. A config edit that changes only them
is still taken on mid-sync, like any other. The built-in steps that read
files are marked by the loader (`UNPINNED_BUILTINS` in `config.rs`:
`keyword_index`, which globs its render tree's `.md` files; `embed` and
`embedding_map`, which read qmd's own SQLite file; and perseus's render,
which reads its TEI files); any step may say `reads` itself.

## What a sink owes its consumers

A step's sink is whatever it writes: a doltlite store, the qmd index, a
directory of markdown. A consumer relies on two separate properties, and
conflating them produces a contract that is false for half the sinks.

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
An index rewritten in place, a directory being emitted and an appended
file do not. So P2 is the step's to declare — a `capabilities` event
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
puppets, runs the loop in-process under `host::run_idle` as the server
does, and plays a person through the store. It waits only on what it
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
`HARNESS_SEEDS=<n>` runs that many (32 by default):

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
doltlite commit hash is one: the store's head moves only when a commit
changed something ([`doltlite.md`](../../../docs/dev/doltlite.md#diffs)). The built-in steps report exactly that, and spell it the
same way on a seal and in the outcome, so finishing on the commit last
sealed moves nothing downstream.

**A step that reports no version gets a new one on every success**
(`<fingerprint>:run-<invocation id>`), so everything that reads it runs
again, as `make` would with no timestamps to compare. That is always safe
and sometimes wasteful; the loop says so on the event stream when it
happens, and a step that wants its readers to skip reports a version.

**A failed or stopped step's reported version stands**: what it committed
before it ended, its consumers read (`plans/supervisor.md` §2.5).
`datalib-step` reports its store's head however it ends, so a commit its
writer published at open, or at the stop, reaches them. One that reports
nothing moves nothing, since its tree may be half-written. The one thing
no report can carry: a step killed outright after publishing and before
saying so reaches its consumers at its next run, not at once.

A step that did not run contributes the version recorded for its output
last time, or `UNKNOWN`, which compares equal to itself so two runs that
both know nothing agree.

**The step's fingerprint is folded into every version.** A step reports on
its content and has no way to know its own definition changed. Without
folding, a bumped `code_version` re-runs the step (its fingerprint moved)
while the reported version stays identical, so consumers skip: the tree is
rebuilt and the index keeps serving what the old definition produced. A
real version therefore always contains a colon (`<fingerprint>:<version>`)
and can never collide with `UNKNOWN`.

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
and the fix is on the group's line. The five warnings: a group nothing
is filed under; a `name` written on a grouped step, whose label comes
from the group; an applet filed under a group that does not exist; a
`keyword_index` that `qmd_aggregator` does not read; and a built-in
step's `common.always_clear_before_ingest`, which no longer does
anything (`datalib-step` drops it before parsing, so the step still
runs). The Manage screen's System row counts the warnings and shows
their words on hover. The retired
shapes — `datalib-step download|render|grid_index|qmd_index` on a
command line, and a built-in `qmd_index` step — are `Rejected`, because
they no longer run, and the diagnostic names `datalib-migrate-config`.
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
  branch, in any process ([`doltlite.md`](../../../docs/dev/doltlite.md#branches-head-and-the-working-set)); two loops on one root would interleave both. While `datalib-http` is up it holds this lock
  for its life and runs the loop in-process whenever a request is open
  (`http/src/supervisor.rs`), so every `datalib-dag` sync is a client of
  it. A `datalib-dag` is never refused for the lock: a sync is a request
  row in `system/supervisor.sqlite`, so it writes its row and follows it
  while whoever holds the lock runs it, trying the lock again whenever
  anything is announced, its release included, in case that loop ends
  first (`docs/dev/plans/supervisor.md` §2.8). Only `--reset`, which empties
  stores, needs the root to itself and is refused while a loop runs —
  always, with the app up; the app runs its own resets between syncs.
- **One server per data root** (`system/lock`), which `datalib-http`
  takes for its own reasons (the API token, the feedback, usage and
  remote-media stores).

They must be *different* files: a server that starts while a
`datalib-dag` runs the loop holds the root as a server and waits for
`runner-lock` — the UI's syncs are rows that loop serves meanwhile — and
takes the loop over when it ends.

Whoever runs the loop owns its steps' processes. Each step holds a pipe
from that process (`DATALIB_PARENT_PIPE`) and stops itself when the pipe
closes, however the process died. A step the loop stops gets SIGINT on
its process group and SIGKILL on it fifteen seconds later if it is still
there (`subprocess::stop_ladder`, `step::STOP_GRACE`), so one that
ignores its SIGINT cannot hold its store for good. A loop that died
holding the lock leaves its run and its invocations open in the record
and the run store; the next process to take the lock closes them
(`supervisor::host::take_over`). Its requests are still open rows, and
the next loop runs them.

`flock(2)` rather than a pid file, because the kernel releases it when the
holder dies — a crashed process leaves no stale lock to reason about. The
file's contents are advisory: they exist so a refusal can name the holder,
and are never trusted to decide whether it is held.

`FileLock::is_held` (`datalib_flock`) is a **read-only** probe, which is
why it is separate from `FileLock::acquire`: acquiring creates the file if absent and rewrites its contents.
Both are right for a process claiming the root and wrong for one merely
asking — a caller on a timer would rewrite the file every few seconds, and a
root that had never run would sprout a lock file from being looked at. It is
racy by nature: the holder may let go a microsecond later. Don't build an
invariant on it.

Read-only does not mean invisible. `flock(2)` has no way to ask without
taking, so the probe holds the lock for an instant, and a server that has
not got the lock yet probes on every change under the root — most often
just as a run starts. A
`--reset` that finds the lock held therefore keeps trying for two seconds
before it refuses; and a probe that found the lock free announces that it
let go, so a sync that met the probe takes the lock at once.

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

The trap is that `done` accumulates across **everything** the step
reported, while `total` is simply whichever length it announced last —
and the runner relabels every event to the step that emitted it, so a
step cannot have two independent bars even if its code looks like it
does. Two bars each announcing their own size therefore pin `queued` at
zero from the second one onward: `done` already carries the first bar's
work, and the subtraction saturates.

So a download reports through one `datalib_etl::progress::RunBar` for
the whole run, whose total only ever grows — each phase adding what it
has learned it will do. Announcing no total at all is fine and means
"size unknown"; the sink then publishes no `queued`, which is not the
same as publishing zero, because zero means finished.

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
It is the only timer, and it exists to catch that bug.

A `datalib-dag` running the loop with no server up has nobody watching
the config, so it takes an edit on at its next busy period, not mid-sync.

The loop's idle side lives once, in `supervisor::host::run_idle`: a busy
period whenever a request is open, requests asked to stop before any
period took them closed as `stopped`, the record settled when the
switches or the config move, then a wait for an announcement, a nudge (in-memory work such as a
reset) or the host's stop. The server's host and the tests both run it.

## The record

The loop's memory is its **record**, in `system/supervisor.sqlite`
(`supervisor/record.rs`), beside the mailbox anyone writes
(`supervisor/store.rs`): `requests` (its `roots`, `opened_by`, a
`stop_requested_by`, and once closed its `outcome` — `done`, `failed`
or `stopped` — and `failed_step`) and `turned_off` (`step`,
`turned_off_by`). It is plain SQLite in
rollback-journal mode, so any `sqlite3` reads it, and only the holder of
`runner-lock` writes it. `supervisor_contention_test` runs seven
processes on one store (people opening requests, the loop saving, the
server reading) and checks that every write lands and that the file's
header says rollback-journal:

| table | one row per | what it holds |
|---|---|---|
| `steps` | step | its state now (`state`, `state_detail`, `turned_off_by`, and `requests`, the open requests it still has work in: a JSON array, NULL once it has none, which is when its Manage row offers Sync again), what it read at its last success (`reads`), under which definition (`fingerprint`), and what happened the last time a run reached it (`last_*`) |
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
for one busy period of the loop (`datalib-dag` takes `--run-id` instead
when given one). `supervisor::host::step_env` puts it in the
child environment and `start_record` hands the same string to the run
store (`system/runs/runs.sqlite`) *before* the loop starts, and `Runner`
reads it back out of that environment, so the record, the store and
every step name one run. If
they diverge nothing errors — the store describes a run nobody is
displaying, `/api/dag` reports no progress on the id mismatch, and the
UI silently shows none. `started_at` stays the pinned
`DATALIB_DAG_NOW`.

The record is the only channel to a reader who did not spawn the run, and the loop
saves it before it closes a request, whether its work is done or it was
stopped, so a reader that sees a request closed never finds a step still
serving it. A step stopped with its request reads Stopping until its
process has exited. `POST /api/requests`
opens one request per group its roots belong to, so each source's sync
has a Stop of its own, and answers only once a step names each new
request (`Store::taken_on`), so the rows read after a Sync already show
them.
