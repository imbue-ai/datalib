# `datalib-dag` — the runner

Reads a `config.toml`, builds a DAG from it, and runs the steps. This file
holds the rules you cannot recover by reading the code; the design history
and the open questions are in
[`docs/dev/pipeline_dag_architecture.md`](../../../docs/dev/pipeline_dag_architecture.md),
and the contract a step author needs is
[`docs/dev/step_protocol.md`](../../../docs/dev/step_protocol.md).

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

A step outside any group is a custom executable and writes its `id`
verbatim. That is the only place a step id is written.

Every step under a group gets the two halves and the type in its
environment — `DATALIB_DAG_GROUP`, `DATALIB_DAG_FUNCTION`,
`DATALIB_DAG_GROUP_TYPE` — beside the composed `DATALIB_DAG_STEP`.
Today the built-in `datalib-step` still dispatches on its argv and
writes `<name>/raw` or `<name>/rendered_md` from the first segment of
the step id; the functions are therefore named `raw` and `rendered_md`
(and `grid`, `qmd`) so that the composed id and the tree the step writes
agree. Dispatching on the environment and naming the tree after the
function is the next slice of
[`docs/dev/plans/groups_and_functions.md`](../../../docs/dev/plans/groups_and_functions.md).

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

## What a run executes, and what makes a step stale

A run executes a **runnable subgraph**: the source steps this run selected
plus everything downstream of them. With no `--sync` that is the whole
graph. Steps outside it are reported `NotSelected` and cannot run, whatever
their state.

The subgraph is reachability in the graph, computed once before anything
runs and deliberately independent of run-time state. That is what makes
"sync yolink" mean the same thing every time — the set of steps that can
move is a property of the config, readable off the DAG, rather than
something you reconstruct from the state file to predict. The cost is that
pending work elsewhere stays pending; it comes back on the next full run,
and in exchange a per-source sync never does surprising work on someone
else's chain.

Steps outside the subgraph are still *walked*, because an in-subgraph fan-in
can depend on them: walking publishes their recorded output versions and
gives every step a terminal status for the report. They are never invoked,
and `NotSelected` is never written into `last_run` — doing so made a
`--sync slack` erase email's record, so a source's "last synced" moved every
time some other source synced.

Inside the subgraph a step runs iff it is **stale**, which is one predicate
with four clauses:

- it declares no inputs (its real input is outside the graph, so it always
  runs), or
- it has never succeeded, or
- some input's version differs from the one it consumed at its last success,
  or
- its own fingerprint — argv, env, declared inputs — differs from the one
  recorded then.

A failed step blocks its dependents *this run*, but any partial output
versions it reported are recorded: steps are incremental, so the next run
resumes from the committed partial state. Failure kinds map to a retry
policy in the scheduler; the step only classifies. Retries simply re-invoke
the step, which is safe because steps promise idempotency.

## Versions are reported by the step, not measured by the runner

A step reports one version string per output. It must be a function of the
output's **content** — a dolt commit hash, a row-set hash, a render cursor's
hash — so that two runs over the same data report the same string and
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

**The step's fingerprint is folded into every recorded version.** A step
reports on its content and has no way to know its own definition changed.
Without folding, a bumped `code_version` re-runs the step (its fingerprint
moved) while the reported version stays identical, so consumers skip: the
tree is rebuilt and the index keeps serving what the old definition
produced.

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
from the group; an applet filed under a group that does not exist; and
a `datalib-step` download or render step outside any group, the shape
written before `[[groups]]` existed — it still runs, and the warning
names `datalib-migrate-config`. A warning passes the strict door too
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

- **One runner per data root.** The scheduler rewrites a single JSON state
  file after every terminal step, and the steps it spawns write raw stores
  whose doltlite working set is shared across processes. Two runners on one
  root interleave both.
- **One server per data root**, which `datalib-http` takes for its own
  reasons (the API token, the job and feedback stores).

They must be *different* files: the server spawns the runner, so one shared
lock would deadlock the server against its own child.

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

## Progress: the accumulator is in the sink, not the bus

`Event::ProgressInc` carries a **delta**. The bus coalesces — of the ticks
between two flushes only the newest is written — and coalescing deltas
silently loses work, turning "347 of 900" into whatever fraction of the
increments happened to land on a flush boundary.

So the running total is kept per step in `progress_bus.rs`, and what reaches
the bus is always an absolute position. Dropping one of those is lossless,
which is what makes the coalescing correct rather than merely cheap.

## The run record

`system/dag_state.json` must carry the plan before anything runs, a terminal
state for *every* step (including ones that were skipped or blocked and
never "ran"), a `finished_at` that distinguishes a completed run from a
crashed one, and per-step timings.

The run id is the pinned `DATALIB_DAG_NOW`, verbatim. `datalib-dag` mints
that value and hands it to the progress bus as the run id *before* calling
`run`, so the two derive the same string independently. If they diverge
nothing errors — the bus describes a run nobody is displaying, `/api/dag`
filters every row out on the id mismatch, and the UI silently shows no
progress at all.

State is saved on `running`, not only on terminal states. That file is the
only channel to a reader who did not spawn the run, so without the
running-state save `dag_state.json` went straight from "not reached yet" to
"succeeded" and pressing Sync looked like nothing had happened until the
step finished.
