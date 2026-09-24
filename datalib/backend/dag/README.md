# `datalib-dag` — the runner

Reads a `config.toml`, builds a DAG from it, and runs the steps. This file
holds the rules you cannot recover by reading the code; the contract a
step author needs is
[`docs/dev/step_protocol.md`](../../../docs/dev/step_protocol.md). One
design question is still open: the node set is known before a run
starts, so a step that *discovers* downstream work (a fan-out per
conversation, say) lives inside one node rather than expanding the
graph.

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
it re-runs every step under the group. Introducing that slot, and
dropping `--outputs` from every argv, moved every step's fingerprint
once: the first run on binaries with `[[groups]]` re-runs the whole
pipeline against an existing root. It converges, and nothing is lost.

A group's `description` — what the source is to its owner, free text —
is treated exactly like its `name`: never forwarded, never fingerprinted.
Nothing reads it yet. It was briefly passed to qmd as the collection's
context, until that turned out to be result metadata rather than a
ranking input (imbue-ai/datalib#409 has the evidence and the options).

A step outside any group is a custom executable and writes its `id`
verbatim. That is the only place a step id is written.

Every step under a group gets the two halves and the type in its
environment — `DATALIB_DAG_GROUP`, `DATALIB_DAG_FUNCTION`,
`DATALIB_DAG_GROUP_TYPE` — beside the composed `DATALIB_DAG_STEP`. A
step with no `command` runs `datalib-step`, which dispatches on that
environment and writes the tree its id names; that is why a built-in
step's function is the directory it writes (`ingest`, `render_markdown`,
`grid_index`, `qmd_index`), and why the loader requires such a step to
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
keeps the invariant, and is why the whole graded-loading change touched no
scheduler code.

Dropping cascades, and the diagnostics distinguish the two cases. A step
whose input names a step that was itself dropped is `Blocked`, not
`Rejected` — nothing is wrong with it, and sending the reader to its line
would send them to the wrong line. Graph assembly is told what the config
pass already threw out so it can tell "you named a step that does not exist"
from "the step you named is broken"; the commonest case of all is a render
step whose fetch step was rejected for a bad key.

Cycle reporting separates the ring from what merely hangs below it. Kahn's
algorithm cannot: it leaves behind everything it could not order, ring and
tail alike, and telling someone a step three hops below a cycle is *in* it
sends them looking for an `inputs` entry that isn't there.

## What the loop runs, and what makes a step stale

The loop runs what open **requests** want. A request is a row in
`system/supervisor.sqlite` naming its **roots** — the steps a Sync was
pressed on, or every source step for `datalib-dag` with no `--sync` —
and who opened it (`by`). Its **scope** is the roots and everything
downstream of them. Anyone may open one, or ask one to stop, or pause a
step: that is a row too. Only the process holding `runner-lock` runs the
loop. It never polls for new rows: every write to the store, from any
process, rings a doorbell — one FIFO per listener in
`system/supervisor-bells/` (`supervisor/bell.rs`) — and so do a config
edit the server sees and letting `runner-lock` go. A SQLite commit
reaches no other connection by itself, and macOS does not announce an
append to a WAL held open. A 5 s backstop looks anyway, and a write it
finds that nobody rang for is a bug, logged at ERROR. A **run** is one busy period of the loop — from taking a
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
the graph, the open requests, the pauses and the facts (each sink's
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
| `paused` | someone paused it; `paused_by` says who |
| `blocked` | wanted, but a producer it reads has never published and is not going to run |
| `failed` | its retries ran out and nothing it reads has moved since |
| `idle`, `stale` | no open request wants it; up to date, or not |

**A wanted step is due** iff it is **stale** — it has never succeeded,
some input's version differs from the one it consumed at its last
success, or its own fingerprint (argv, env, declared inputs) differs
from the one recorded then — or it declares no inputs and has not run
since this request opened: a source's real input is outside the graph,
so every request runs it once.

A due step waits, and says why, for one of four things:

- **its upstream**: a producer it reads is running and does not declare
  `streams_output`, so what it writes may be half-done, or is about to
  run, held only by a budget, its sink or a reader. A running producer
  that streams lets its consumers start on each seal as it lands, and
  staleness keeps them from running when nothing new has. A producer
  waiting on its own upstream holds nobody back, so a fan-in never waits
  for its slowest source.
- **its sink**: another writer of the same tree is running.
- **a reader**: a step that reads its tree off disk, not at a pinned
  commit, is running.
- **a budget**: each step counts against one of three — `network` for a
  source, `cpu` for a grouped step with inputs, `index` for one with no
  group type — so a download waiting out a rate limit never keeps a
  render from starting. `--parallelism N` is N downloads and N renders;
  the two index steps may always run beside each other.

**A step that reads its inputs off disk never overlaps a writer of
them.** The loader marks the built-in ones (`UNPINNED_BUILTINS` in
`config.rs`: the qmd index, which globs render trees' `.md` files, and
perseus's render, which reads its TEI files): such a step waits for a
running producer even if it streams, and a writer of what it reads waits
for it to finish.

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
checkpoints and exits, and until it has, its row reads Stopping. **A
pause** keeps a step from starting and stops it if it is running; a
paused step a request skipped takes no part and records no run. A pause
made while the loop is idle reaches the record through
`Runner::settle`, one tick with nothing open. A request naming a step
no config the loop has taken on has waits, with its roots recorded as
waiting on it, for one that has it.

**The loop re-reads the config while it runs.** A source added mid-sync
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
grid, and its next Sync downloads everything again. The design, and what
is still to come (two steps writing one tree), is
[`plans/supervisor.md`](../../../docs/dev/plans/supervisor.md).

## Versions: read from the store, or reported by the step

**A step whose tree holds doltlite stores is versioned by the runner.**
After every invocation, and at every checkpoint, it reads the commit each
store's `main` is at (`sink.rs`): `<store file>:<hash>` for each
`*.doltlite_db` directly in the tree, in name order. That is exactly what
a pinned reader of the store can see, so it is exactly what a consumer
reads; what the step reports is not consulted. It is read whether the step
succeeded or failed, because a writer's `open` publishes a commit its
crashed predecessor left. Only the stores at the top of the tree count —
a render tree's per-document directories hold markdown.

**Any other step reports one version string per output.** It must be a
function of the output's **content** — a row-set hash, a cursor's hash —
so that two runs over the same data report the same string and
"unchanged" is something the scheduler *derives* rather than something a
step asserts. A timestamp does not qualify. The value is otherwise opaque:
the runner only ever compares it for equality.

An output the step says nothing about is content-hashed instead. That is
`tree_version`, whose only caller in this crate is `resolve_outputs`, and it
is only ever reached for a step that just ran — hashing is always correct and always slower, since it
reads every file under the output. When it fires it says so on the event
stream: an unreported version costs a full read of the tree, and #225 is the
case for what a slow path nobody can see costs in the end.

The runner never reads a tree to version it on a step's behalf. A step that
did not run contributes the version recorded for its output last time, or
`UNKNOWN`. Reading gigabytes to answer a question a step can answer from a
commit hash — for work this run already decided not to do — is the thing
that policy exists to prevent.

**The step's fingerprint is folded into every reported version.** A step
reports on its content and has no way to know its own definition changed.
Without folding, a bumped `code_version` re-runs the step (its fingerprint
moved) while the reported version stays identical, so consumers skip: the
tree is rebuilt and the index keeps serving what the old definition
produced. A version read from a store is not folded: a rebuild that
changes rows is a new commit, and one that changes nothing leaves nothing
new to read.

`ABSENT` and `UNKNOWN` are compared for equality like any other version,
which gives the right answer in both directions. A tree that was never
produced and still isn't compares equal to itself, so a consumer that
already recorded it is not dirtied; one that existed and was deleted moves
to a different string, so its consumers re-run. A real version always
contains a colon (`<fingerprint>:<version>`), so it can never collide with
either sentinel.

## Diagnostics: severity is blast radius, not mood

The loader returns a list of diagnostics rather than an `Err`, because the
first-problem-wins version meant one stray key in one step took down the
grid, search, the document view and every applet — the applets are declared
in the same file (#209).

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
and the fix is on the group's line. The warnings today: a group nothing
is filed under; a `name` written on a grouped step, whose label comes
from the group; and an applet filed under a group that does not exist.
The retired shape — `datalib-step download|render|grid_index|qmd_index`
on a command line, from before `datalib-step` read its function from
the environment — is `Rejected`, because it no longer runs, and the
diagnostic names `datalib-migrate-config`. A warning passes the strict door too
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
  branch, in any process; two loops on one root would interleave both. While `datalib-http` is up it holds this lock
  for its life and runs the loop in-process whenever a request is open
  (`http/src/supervisor.rs`), so every `datalib-dag` sync is a client of
  it. A `datalib-dag` is never refused for the lock: a sync is a request
  row in `system/supervisor.sqlite`, so it writes its row and follows it
  while whoever holds the lock runs it, trying the lock again every half
  second in case that loop ends first (`supervisor/store.rs`,
  `docs/dev/plans/supervisor.md` §2.8). Only `--reset`, which empties
  stores, needs the root to itself and is refused while a loop runs —
  always, with the app up; the app runs its own resets between syncs.
- **One server per data root**, which `datalib-http` takes for its own
  reasons (the API token, the feedback, usage and remote-media stores).

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

`is_held` is a **read-only** probe, which is why it is separate from
`acquire`: acquiring creates the file if absent and rewrites its contents.
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
before it refuses; a sync that meets a probe follows for half a second and
takes the lock on its next try.

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
leaves. `queued` is the "N queued" the Manage screen shows.

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

## The record

The loop's memory is its **record**, in `system/supervisor.sqlite` beside
the requests and pauses (`supervisor/record.rs`). It is plain SQLite, so
any `sqlite3` reads it, and only the holder of `runner-lock` writes it:

| table | one row per | what it holds |
|---|---|---|
| `steps` | step | its state now (`state`, `state_detail`, `paused_by`, and the open `request` it serves), what it read at its last success (`reads`), under which definition (`fingerprint`), and what happened the last time a run reached it (`last_*`) |
| `sinks` | tree a step writes | the version it last published |
| `runs` | busy period of the loop | when it started and finished |
| `run_steps` | step the newest run has reached | what it is doing in that run |
| `invocations` | process the loop started | when, in which run, and how it ended (`outcome` is NULL while it runs) |

A run must record a state for *every* step in scope (including ones that
were skipped or blocked and never "ran"), a
`finished_at` that tells a completed run from a crashed one, and per-step
timings. The loop holds the record in memory (`record::Record`) and
saves only what changed since its last save (`record::changes`), after
every tick and every event; a Manage row's Status is `steps.state`,
read straight from it.

The run id is `DATALIB_DAG_RUN_ID`, verbatim — a UUID v7 the host mints
for one busy period of the loop (`datalib-dag` takes `--run-id` instead
when given one). `supervisor::host::step_env` puts it in the
child environment and `start_record` hands the same string to the run
store (`system/runs/runs.sqlite`) *before* the loop starts, and `Runner`
reads it back out of that environment, so the record, the store and
every step name one run. If
they diverge nothing errors — the store describes a run nobody is
displaying, `/api/dag` filters every row out on the id mismatch, and the
UI silently shows no progress at all. `started_at` stays the pinned
`DATALIB_DAG_NOW`.

The record is saved on every tick, not only on terminal states. It is
the only channel to a reader who did not spawn the run, and the loop
saves it before it closes a request, so a reader that sees a request
closed never finds a step still serving it. `POST /api/requests`
answers only once a step names the new request (`Store::taken_on`), so
the rows read after a Sync already show it.
