# Joining a running sync: a source started during a run starts now

**Status: proposal (2026-09-18), not built.** Nothing below describes
the tree; §1 describes what the tree does today and was checked against
it at `ae2d52f0`. Where this doc and the tree disagree, the tree wins.

## 0. The problem

Add two sources while a third is syncing, press Sync on each, and both
show an hourglass until the first sync ends. Measured on a real root on
2026-09-18: `work-gmail/ingest` started at 22:04:42; a job for
`work-slack/ingest` was enqueued at 22:09:09 and one for
`work-fastmail/ingest` at 22:10:20; at 22:12 both were still `pending`
with nothing in the run log about either, while the runner had three of
its four parallel slots idle.

The runner is parallel *within* a run (`Runner::parallelism`, four
slots; a streaming budget on top). The wait is one layer up, and it is
there on purpose in two places:

- **The worker runs one job at a time.** `worker::run` claims a job,
  spawns `datalib-dag`, and `await`s its exit before claiming the next
  ([`worker.rs`](../../../datalib/backend/http/src/worker.rs), the loop
  around `claim_next_job`).
- **The runner fixes its subgraph before anything runs.** "The set of
  steps that can move is a property of the config, readable off the
  DAG" ([dag README § What a run executes](../../../datalib/backend/dag/README.md)).
  It also holds `system/runner-lock`, one per root, because
  `dag_state.json` is one JSON file rewritten after every terminal step
  and a raw store's doltlite working set is shared across processes
  ([dag README § Two locks](../../../datalib/backend/dag/README.md)).

There were two ways out. Running a second `datalib-dag` per job means
giving up the one-runner-per-root invariant and everything that leans
on it: `dag_state.json` becomes a contended read-modify-write,
`current_run` becomes plural, the fan-ins `unified_index/grid_index`
and `qmd_index` need a cross-process step lock, and the tests that
prove it are the two-process, timing-dependent kind this repo has
learned to distrust. This plan takes the other way: **a job that
arrives while a run is in flight joins that run.** The runner keeps
its lock and its state file; the fan-in re-run it needs is machinery
the scheduler already has for streaming.

## 1. What exists that this builds on

Checked against the tree. Each is a reason the join is cheaper than it
looks.

- **The scheduler already re-runs a step after it "finished".** A
  consumer whose producer seals a checkpoint gets another pass;
  `in_flight`, `final_pass_owed` and `streaming_pass_owed` in
  `scheduler.rs` (`Runner::run`, around the dispatch loop) enforce at
  most one instance of a step in flight and queue the pass it is owed
  when the current one lands. Reopening a terminal fan-in for a new
  producer is the same move.
- **Staleness does the rest.** A reopened `grid_index` runs iff an
  input's version moved since it last consumed it. `work-slack/render_markdown`
  has never run, so its recorded version is `UNKNOWN`; once it runs,
  the fan-in's input moves and the ordinary predicate fires. No new
  rule.
- **A control channel exists and is silent.** The worker spawns the
  runner with stdin as a pipe and writes nothing to it; its closing is
  the "parent gone" signal (`datalib_parent_watch::exit_with_parent`).
  A line protocol on that pipe costs no new file, socket or port.
- **`NotSelected` is never written into `last_run`** (`Runner::finish`),
  so a chain the run walked past and then joins has no false history
  to undo. Only the in-memory `status[]` and `current_run.states`
  carry the `not_selected` and both are this run's to change.
- **The Manage grid already derives "queued" from the job queue, not
  from the runner** (`http/src/manage/status.rs`, the `claim` branch),
  and already knows a job's id is its run's id (`spoken_for`). Joining
  changes one lookup there, not the derivation.
- **`sync_jobs.parent_job_id` is dead.** Its doc says "New rows are
  always NULL; kept so old rows still render." There are no real
  users; it goes.

## 2. The design

### 2.1 What a join is

A **join** adds source steps to a run in flight. The runner:

1. **Reloads the config** and rebuilds the `Graph`. The running graph
   was built when the run started; in the motivating case the joined
   source did not exist then. Every step in flight must exist in the
   new graph with the same fingerprint, or the join is **refused** —
   the config changed under a running step and this run cannot
   honour that. A refused join costs nothing: the job stays `pending`
   and runs next, exactly as today.
2. **Widens the runnable subgraph** to reach(old roots ∪ new roots),
   the same reachability `runnable_subgraph` computes today.
3. **Reopens** every step in the new reach that already has a terminal
   status. Its `status[i]` goes back to `None`, its entry in
   `current_run.states` is removed, and `remaining_deps[i]` is
   recomputed as the number of its deps with no status. A step with no
   remaining deps goes on `ready`. A step in flight is not touched; it
   is marked `final_pass_owed`, and the completion handler queues the
   pass when the current one lands — the existing path.
4. **Records the join**: `current_run.plan` and a new
   `current_run.joined: [{at, steps}]` in `dag_state.json`, saved
   before the first reopened step is dispatched, so a reader sees the
   run grow before it sees a new step running.
5. **Answers** on stdout with one line: `datalib-dag: join ok <ids>` or
   `datalib-dag: join refused <reason>`. The worker already pumps
   stdout for its failure tail; it learns to recognise this prefix.

A root that is *already in the run* — Sync pressed on Gmail while Gmail
runs — is a join too: the step is in flight, so it is owed another pass
and gets one when this one lands. Today that job waits for the whole
run and then re-runs everything downstream; joined, it re-runs the
ingest as soon as the ingest is free.

### 2.2 The line protocol

stdin carries newline-terminated lines: `join <id>[,<id>…]`. EOF still
means the parent is gone. `datalib_parent_watch` gains a variant that
hands each line to a callback (or a channel) instead of discarding
bytes; the EOF behaviour is unchanged. The scheduler loop `select!`s
on that channel beside `checkpoints` and `set.join_next()`.

A join that arrives after the loop has decided to exit (`running == 0`)
is lost: the process is on its way out. The worker treats a missing
answer as a refusal — the runner exits, the job is still `pending`, the
loop claims it. The race resolves to today's behaviour.

### 2.3 The worker

`run_job` today polls `child.try_wait()` and the job row for a cancel.
It also polls for **pending jobs** (a `peek_pending` alongside
`claim_next_job`; peek does not mark anything). For each: write the
line, wait for the answer with a short deadline, and on `ok` claim the
job and set its `run_id` to the host job's id (§2.4), its `pid` to the
host's, its progress to "joined the sync of …". On `refused`, leave it.

Joined jobs finish with the host run and take its outcome — done,
failed, canceled. Cancel on a joined job cancels the run; that is what
the help text already promises ("one job is one runner process over a
whole subgraph, so stopping is per sync, not per row"), and per-step
cancel is not in this plan.

### 2.4 The job schema

Replace `sync_jobs.parent_job_id` with `run_id VARCHAR(36) NOT NULL`,
equal to `id` unless the job joined a run, in which case it is the host
job's id. Everything that said "a job's id is its run's id" says
"a job's `run_id`" instead:

- `manage/status.rs` `spoken_for` compares `run_in_flight.run_id` to
  `claim.run_id`.
- The worker passes `--run-id` the host job's id, as now; a joined
  job's steps write `step_runs` rows under that run id, which is what
  `/api/dag` filters on, so the joined source's progress appears on
  its rows with no change to the readers.
- `JobProgressEvent` gains `run_id` so the UI can group the queue by
  run if it wants to; no UI change is required for the rows to read
  right.

Breaking: an old `jobs.doltlite_db` does not load. Say so in the commit.

### 2.5 One clock

`DATALIB_DAG_NOW` is pinned per run, and a joined step inherits it: a
Slack ingest joined at 22:09 stamps its `*_at_utc` columns 22:04. That
is the existing rule ("one clock for the whole run … so a run's
timestamps agree with what its steps stamped") and this plan keeps it.
The join time is recorded on the run (`joined[].at`), which is where a
reader who cares can find it.

## 3. Hazards

- **Reopen touches a dozen parallel `Vec`s indexed by step.** The
  loop's state is `status`, `remaining_deps`, `ready`, `in_flight`,
  `final_pass_owed`, `streaming_pass_owed`, `early`, `streams`,
  `attempts_taken`, `errors`, `versions`, `changed_now`, `queue`
  (`QueueLedger`), and the graph grows under all of them. The first
  slice below moves them into one struct with a `remap(old, new)` by
  step id, before any join logic exists, so the growth is one place.
- **A wrong `remaining_deps` is a hang or a half-read.** Too high and
  the fan-in never becomes ready and the run never ends; too low and
  it dispatches against a render still being written. The test that
  guards it has to be watched failing against a deliberately wrong
  recount, per AGENTS.md.
- **`versions` for a never-run producer is `UNKNOWN`.** Reopening a
  fan-in whose new input is `UNKNOWN` must leave it stale, not skip it;
  the Skip branch of the dispatch loop inserts that placeholder and
  `decide` treats a differing version as stale, so this holds — but it
  is a claim to test, not assume.
- **The fan-ins must not be read by anyone as "done for this run"**
  between the join and their re-run. `current_run.states` drops their
  entry on reopen; `step_runs` in `runs.sqlite` keeps the last terminal
  row until the new `running` lands. The Manage grid reads the queue
  first (§1), so it shows the joined rows as queued and the fan-in as
  its last state, which is true.
- **Config reload can refuse.** A joined-while-editing config — the
  user changes Gmail's params while Gmail runs, then adds Slack — is
  refused and the Slack job waits, with the reason on the job row.
  That is the right answer and the message has to say why.
- **Doltlite is not made worse.** The join is in-process; every store
  still has one runner's steps as its writers, one instance per step
  in flight. Nothing here needs `doltlite_two_process_test`.

## 4. Order of work

Each slice is a PR; each is green on its own.

1. **Scheduler: the loop state as one struct.** Pure refactor of
   `Runner::run`: the per-step `Vec`s and the two queues into a
   `LoopState` with `new(graph)` and `remap(old: &Graph, new: &Graph)`.
   No behaviour change; the existing 34 scheduler tests are the
   proof.
2. **Scheduler: `reopen(roots)` on a fixed graph.** Widen the
   subgraph, reopen terminal steps in the new reach, mark in-flight
   ones owed. Tests: a `NotSelected` chain joins and runs; a finished
   fan-in re-runs once its new input lands and is skipped when nothing
   moved; a join naming an in-flight root gets exactly one more pass;
   a join naming a step that is not a source step is refused; the
   `remaining_deps` recount, watched failing.
3. **Scheduler: reload and grow.** `Graph::build` from the re-read
   config, the in-flight fingerprint check, `remap`, then `reopen`.
   Tests: a source added to the config mid-run joins; a changed
   in-flight step refuses with its id in the reason.
4. **`datalib_parent_watch`: lines, not just EOF**, and the runner's
   `select!` arm plus the stdout answer. `datalib-dag` gets nothing
   new on its command line.
5. **Jobs: `run_id` replaces `parent_job_id`**; `status.rs` follows;
   `JobProgressEvent` carries it. The `status.rs` tests that pin
   "pending behind another job's run" gain the joined case.
6. **Worker: peek, join, finish-with-host.** Tests in `worker.rs`'s
   existing fake-repo style: a pending job arriving mid-run is joined
   and ends with the host; a refused join stays pending and is claimed
   next; a join whose runner exits before answering is claimed next;
   cancel on a joined job stops the run.
7. **Docs.** The dag README's "One runner per data root" paragraph
   gains the join; `agent_user.md` § Running a sync says a Sync pressed
   during a run starts now rather than after; this file moves to
   `completed/` or is deleted.

## 5. Out of scope

- Per-step cancel inside a run.
- A second runner per root. If it is ever wanted, it is the
  `dag_state.json` redesign this plan avoided, and it should be its
  own plan.
- A per-join clock. One clock per run stays the rule.
