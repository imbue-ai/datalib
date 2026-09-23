# What a cancel leaves behind, and what the log says about it

**Status: PRs 1 to 4 and 8 landed (#682, #686, #692, #697, #700); 5 is
void and 6 and 7 are open. PRs 3, 4, 5 and 8 were none of them what this
doc first said they were — each says so in its own section. Last read
against the tree 2026-09-23.** §1 is what a real data root actually contained — every
number in it was read out of `/Users/thad/datalib/z14` at build
`787c1a4c`, not inferred. §2 is the work, one section per change.
Where this doc and the tree disagree, the tree wins.

This came out of the first careful read of a live log store. The root
had been running for about forty minutes: three sync runs over Slack,
Gmail and Fastmail, two of them cancelled from the Stop button in the
UI. The store held 11,131 lines.

## 0. The short version

The cancel path works exactly as designed right up to the last step,
and then stops. A download hears the interrupt and checkpoints in under
a second. But nothing ever kills a step that does not stop on its own,
so the `qmd_index` step and the `node qmd embed` it spawned outlived
the entire application — and because the runner that records a step's
output was already dead, they finished their work with no trace
anywhere that it happened.

Around that, six smaller things: a "Slack sync failure" that is really
a size limit the user configured, a Gmail fetch failure that reaches
only the log, a store guard that warns about a root being created, and
three sources of volume that make the log harder to read than it needs
to be.

## 1. What the store actually contained

### 1.1 A cancel stops the download and nothing else

Both cancels behave identically:

| | cancel #1 | cancel #2 |
|---|---|---|
| `POST …/cancel` | 13:29:56.43 | 13:40:26.86 |
| `gmail/ingest` checkpointed and exited | 13:29:57.18 (**0.75 s**) | 13:40:27.62 (**0.76 s**) |
| `sync_jobs.finished_at_utc` | 13:30:12 (**+15.7 s**) | 13:40:42 (**+15.2 s**) |

The download half is exemplary. `gmail/ingest` logged `interrupted;
stopping at the next consistent point`, sealed a checkpoint, held its
cursor (`gmail_cursor_held`), declined to treat unlisted messages as
deleted, and `scope_config` correctly kept the prior record because the
run had not satisfied its config. All of that inside one second.

The fifteen seconds is `CANCEL_GRACE`
([`worker.rs:90`](../../../datalib/backend/http/src/worker.rs)) expiring.
And the process it expires on behalf of is not killed by it.

**The orphan.** At the time of the second cancel, `unified_index/qmd_index`
was embedding 598 Gmail documents. After the cancel, after the runner was
killed, after `datalib-http` logged `terminated, shutting down` at
13:44:21 and the whole desktop app exited — this was still running:

```
  PID  PPID  ELAPSED  %CPU  COMMAND
78828     1    15:12   0.0  datalib-step --inputs ["slack/render_markdown", …]
78854 78828   15:10   12.1  node …/qmd.js embed
```

`PPID 1`: reparented, owned by nobody. It was not hung — it ran the
embed to completion and exited cleanly at 15:51, about nineteen minutes
after the cancel and seven minutes after the application that started it
had gone. That is the part worth sitting with. For those seven minutes
the process was writing `unified_index/qmd_index` with no owner, and
anything that started a new sync would have opened the same index as a
second writer. It also wrote no record of itself: the runner unwraps a
step's stderr into the log store, and the runner was dead, so there is
no log line, no `step_runs` row, and nothing in `dag_state.json` saying
this work ever ran.

**Why.** Three pieces that are each individually reasonable:

- [`worker.rs:479`](../../../datalib/backend/http/src/worker.rs) sends
  exactly one `SIGTERM`, then after `CANCEL_GRACE` calls `child.kill()`
  — `SIGKILL`, on `datalib-dag` alone.
- [`datalib_dag.rs:262`](../../../datalib/backend/dag/src/bin/datalib_dag.rs)
  forwards the first signal to its steps as `SIGINT` and only calls
  `subprocess::kill_children()` when `interrupts >= 2`. That second
  signal never arrives, and `SIGKILL` runs no Rust. So
  [`kill_children`](../../../datalib/backend/dag/src/subprocess.rs), whose
  own comment promises *"a step that ignored its SIGINT must not outlive
  the runner, holding its store open"*, describes a path a UI cancel
  cannot reach. The `PARENT_GONE_GRACE` escalation beside it cannot help
  either — it fires when the parent dies, and here the parent is the one
  doing the killing.
- Steps are spawned without a process group of their own
  ([`subprocess.rs:206`](../../../datalib/backend/dag/src/subprocess.rs)),
  so even a working `kill_children()` would signal `datalib-step` and
  miss the `node` process underneath it.

**What it leaves in the run store.** Both cancelled runs have
`runs.finished_at_utc` NULL — a `SIGKILL`ed runner cannot close its own
run. `unified_index/qmd_index` sits at `state = 'running'` in
`step_runs` forever. Four more step processes ended with neither an
`exit_code` nor a `signal`, so the record cannot say how they died.

**And the cancel is invisible in the log.**
[`worker.rs`](../../../datalib/backend/http/src/worker.rs) has no
`tracing` call anywhere on the cancel path — not for the `SIGTERM`, not
for the grace expiring, not for the `SIGKILL`. The only trace of a
cancel in 11,131 lines is the bare `POST …/cancel 204` request line.

**There is already a test for most of this.**
[`ingested_tng_test.py:1647`](../../../tests/fixtures/ingested_tng_test.py)
asserts that no step process lacks an end and an exit, *"a streaming
pass included"*. The real root has six rows that violate it. The fixture
never cancels, so the test cannot see them. The neighbouring assertion at
`:1657` — no line may have an empty `target` — is violated 199 times in
the real root by the qmd-indexer's plain stdout, which the fixture never
produces because
[`run_sync_pipeline.py:374`](../../../tests/fixtures/run_sync_pipeline.py)
skips the qmd work. Both rules are right. Both are unreachable from the
fixture.

### 1.2 The "Slack sync failures" are a configured size limit

`slack/ingest` succeeded in all three runs. The ten warnings on its
Manage row are ten rows in its `problems` table:

```
size 25107330 > limit 5000000    outcome=ok   reason=fetch_failed   severity=warning
size 15529700 > limit 5000000    …  (ten rows, 5.4 MB – 25 MB)
```

Every one is an attachment larger than the `blob_size_limit_bytes =
"5 MB"` in the root's own config. The skip at
[`slack/ingest/api.rs:281`](../../../datalib/backend/etl/providers/slack/src/ingest/api.rs)
goes through `attach.add_failed`, which lands in
[`doltlite_raw.rs:1709`](../../../datalib/backend/etl/src/doltlite_raw.rs)
where `Reason::FetchFailed` is hardcoded and the severity comes from
*"did an earlier run fetch this?"*. A deliberate policy skip is filed as
a fetch failure, and that is what reaches the user.

Note that the codebase already holds the right rule:
[`Severity::default_for`](../../../datalib/backend/problems/src/lib.rs)
maps `Outcome::Ok` to `Severity::Info`. These rows are `Outcome::Ok` and
`Severity::Warning` because this one site overrides it.

The only genuine Slack trouble in the whole log was three `HTTP 429`
responses on `conversations.replies`, each with a server `Retry-After:
10s`, all retried successfully.

### 1.3 A Gmail message that fails to fetch reaches only the log

[`gmail_api/mod.rs:763`](../../../datalib/backend/etl/providers/email/src/ingest/gmail_api/mod.rs)
and the `gmail_ingest_failed` site below it both `warn!` and `continue`
without touching `summary.problems`, which is fed only from
`resolved.problems`. The root has such a warn line and an **empty**
`problems` table in `gmail/ingest/entities.doltlite_db`. That is the rule
in AGENTS.md § *"An error or a warning about a record goes through
`problems`, never only to the log"*, broken.

Nothing is silently lost — `messages_failed` holds the cursor, so the
next run retries. But no screen ever says which message, or that one
existed.

One trap for whoever fixes it: the warn actually caught in this root was
`interrupted while waiting to retry`, which is a cancel, not a failure.
A naive fix mints a false problem row on every cancel.

### 1.4 The downgrade guard warned about the run store once

**Corrected 2026-09-23. The first reading of this, below, was wrong;
what is left is much smaller.** The first warning in the file:

```
downgrade guard: could not read this store's _datalib_meta; not counting it
  store: …/system/runs/runs.sqlite
  error: probe _datalib_meta: (code: 26) file is not a database
```

It was read as an engine mismatch —
[`guard.rs`](../../../datalib/backend/store_meta/src/guard.rs) appends
`runs.sqlite` to the stores it inspects and opens it with
`datalib_pin::open_reader`, the doltlite reader, on a plain SQLite file —
and so as something that failed on every launch. **Doltlite's default
engine reads a plain SQLite file perfectly well**, measured both ways
against this very file:

```sh
datalib-doltlite -readonly .../runs.sqlite "select count(*) from _datalib_meta"  # 6
datalib-doltlite -readonly "file:.../runs.sqlite?doltlite_engine=sqlite" "…"      # 6
```

What the timestamps say instead: the warning is at `13:04:10.727`, two
milliseconds after `data root: /Users/thad/datalib/z14 (created)`. The
guard probed the run store in the instant it was being created. This log
covers one server launch, so "on every launch" was never supported by
it.

The file is perfectly readable and its `_datalib_meta` is well formed:
`schema_version = 9`, `store_kind = runs`, `datalib_version = 0.35.2`.
The remaining fault is only that a root being created warns about
itself.

### 1.5 Sealing, incremental render and the index do work

Worth recording, because it was the other open question and the answer
is good. From the last run's metrics:

```
gmail/ingest             checkpoints                        = 4
gmail/render_markdown    queued from=gmail/ingest           = 3  → documents_rendered = 792
unified_index/grid_index markdowns_loaded = 598, rows_inserted = 1240, problems_copied = 10
unified_index/qmd_index  queued from=gmail/render_markdown  = 792   ← never drained
```

And the qmd passes show incrementality directly — two passes 65 seconds
apart:

```
13:24:48  [1/3] slack   Indexed: 187 new, 1 updated,  858 unchanged  → 188 hashes, 686 chunks, 1m1s
13:25:53  [1/3] slack   Indexed:   0 new, 0 updated, 1046 unchanged  ← the 188 are now unchanged
13:25:53  [3/3] gmail   Indexed: 198 new                             ← checkpoint #1, three seconds old
```

That 198 is Gmail's first sealed checkpoint travelling seal → render →
index in three seconds. The grid index drained its queue to zero and
copied the ten problems downstream.

The one hop that never completes is qmd, and only because every cancel
kills the embed mid-flight. `dag_state.json` records it honestly:
`unified_index/qmd_index` has `input_versions` for fastmail and slack
but **not** `gmail/render_markdown`, while `grid_index` has all three.
The grid has the Gmail rows; semantic search over Gmail has never been
built. It heals on the next uninterrupted run.

While there: `dag_state.json` records in-flight streaming steps as
`"status": ""` with `"attempts": 0`, next to `"succeeded": true` and a
real fingerprint. An empty string is not a `RunState`, and it
contradicts the flag beside it.

### 1.6 Volume

The request log is 4,887 of 11,131 lines — 44% of the store.

| path | n | per minute, syncing | per minute, idle |
|---|---|---|---|
| `/api/manage/rows` | 2,528 | ~140 | 6 |
| `/applet/unified_index/problems` | 927 | 426 (peak) | 0 |
| `/api/pipeline/storage` | 277 | ~11 | 6 |
| `/api/processes` | 251 | ~64 | 0 |

The `/api/processes` figure is the interesting one.
[`RunLogPanel.ce.vue:824`](../../../datalib/ui/src/components/RunLogPanel.ce.vue)
refetches the whole process list (`limit: 1000`) on every `runs` frame,
and [`tables_of`](../../../datalib/backend/http/src/watch.rs) maps
`StepRuns → Runs`, so every progress-message tick — a step updating
`loaded 598/598` — triggers one. The comment beside the call says *"a
step's new attempt is a new process for the picker to offer"*, which is
the right rule; it is not what the code does.

`/api/manage/rows` at ~140/min is the 300 ms debounce working as
designed, so that one is a judgment call rather than a defect.

Two more:

- `upserted a batch of N rows into <table> in Nms` — 2,593 debug lines
  forming about a hundred distinct messages, because the count and the
  table are interpolated into `msg` when they are already fields.
  AGENTS.md § *"Fields, not interpolation"*.
- Gmail's adaptive quota walks 5000 → 4000 → 3200 → 2560 → 2048 → 1638
  → 1310 (floor 1250) in roughly eight minutes, resets to 5000 on the
  next run and walks down again — eleven `gmail_quota_lowered` warnings
  across two runs.
  [`api.rs:133`](../../../datalib/backend/etl/providers/email/src/ingest/gmail_api/api.rs)
  only ever cuts; there is no recovery upward. If it converges to the
  floor every time, either the 5000 default is wrong or the ceiling
  should climb back when Google stops pushing back. **This one needs a
  decision before it needs a patch.**

### 1.7 Two stores growing without a bound

- **The Slack event tape is on by default.**
  [`source_common/src/lib.rs:118`](../../../datalib/backend/source_common/src/lib.rs)
  — `None → enabled`. It wrote 11 MB of JSONL into the data root
  (`messages.jsonl` alone is 8.8 MB), mirroring every upsert. Nothing
  reads it and the config never asked for it.
- **`disk_usage` has no retention.** 8.9 MB after forty minutes: fifteen
  paths sampled every ~7 s while a run holds the root, or about 15 MB
  per hour of active syncing. `run_history` covers runs and process logs
  only, and
  [`app_store.rs:808`](../../../datalib/backend/core/src/app_store.rs)
  (`disk_usage_keeps_every_sample_of_a_series`) is an explicit test that
  every sample is kept.

## 2. The work

Seven changes. The first two are the bug that prompted this and should
land together; the rest are independent and can go in any order.

### PR 1 — A cancel stops everything, and says so

Three parts, all in the cancel path.

**Make the escalation reachable.** At `CANCEL_GRACE`, send a *second*
`SIGTERM` rather than going straight to `SIGKILL`, and keep the
`SIGKILL` as a third step a few seconds later. That makes the runner's
own `kill_children()` the thing that actually runs, which is what it was
written for.

**Give each step its own process group.** Spawn with `process_group(0)`
at [`subprocess.rs:206`](../../../datalib/backend/dag/src/subprocess.rs)
and signal `-pgid`, so a step's whole subtree goes with it. This is the
change that makes the orphan impossible rather than merely unlikely —
without it, a `node` grandchild survives any signal aimed at its parent.

**Log the cancel.** Three `tracing` lines in
[`worker.rs`](../../../datalib/backend/http/src/worker.rs) — SIGTERM
sent, grace expired, SIGKILL sent — each carrying `job` and `pid` as
fields. The next person reads this off the log card instead of off `ps`.

**Test.** Add a cancelled run to the fixture bake, along the lines
[`http_driven_e2e.md`](http_driven_e2e.md) already proposes (*"the first
sync is crashed, then stopped, then finished"*). That single addition
turns `_assert_run_store_hygiene` into a real regression test for all of
this, and picks up the six open step processes for free. Worth checking
in the same pass whether the fixture can stop skipping the qmd work, so
the empty-`target` assertion covers the qmd-indexer's stdout too — if it
cannot, that assertion should say out loud what it does not reach.

### PR 2 — A cancelled run closes its own books

Worth having even after PR 1: a `SIGKILL`ed runner can never close its
run, so `datalib-http` has to. It already has the machinery — the
startup recovery in
[`worker.rs`](../../../datalib/backend/http/src/worker.rs) reconciles
jobs whose runner died. Extend it to the run store: when the worker
finishes a job whose `runs.finished_at_utc` is NULL, close the run and
mark any `step_runs` still `running` as `stopped`.

Today both cancelled runs are open forever and `unified_index/qmd_index`
reads `running` in a store whose process has been dead for an hour.
That is what the Manage screen joins against.

### PR 3 — A size-limit skip is not a fetch failure — **done**

`Reason::OverSizeLimit`, and a skip reaches the `problems` table as
`Outcome::Ok` / `Severity::Info` with that reason, where a failure on a
never-fetched record still reads `Dropped` / `Error`.

**The bookkeeping is deliberately unchanged**, which is the part worth
knowing. A skip still writes `last_error`, and `failed_ids` selects on
`last_error IS NOT NULL` — so the blob stays eligible and raising
`blob_size_limit_bytes` picks the file up on the next run. That was the
open question, and the answer is that the retry behaviour was already
right; only the label was wrong.

`record_object_attempt` keeps its 20 callers. The reason arrives through
`CasEdgeAccumulator::add_skipped` beside `add_failed`, and
`record_object_skipped` beside `record_object_error`; the bookkeeping
half is now `record_object_bookkeeping`, shared by both.

The `media` provider has the same shape
(`the_payload_ceiling_leaves_null_and_is_counted`) and is still worth
checking.

### PR 4 — A Gmail fetch failure reaches the `problems` table — **done**

**No schema change, which this doc twice got wrong.** It first said
`summary.problems`, which is the wrong channel — `DownloadProblem` is
keyed on `setting` / `value` and describes a *configured entry* upstream
does not have. It then said a `gmail_messages_bookkeeping` sidecar and a
minor version bump. Neither was needed:

- An absent table is simply created on open; only the store's cursors
  are cleared, so the next run walks from the start (`etl/README.md`
  §"Schema self-healing"). No refusal, no ladder rung, no bump.
- But `gmail_messages` is the wrong table for a sidecar anyway. Its own
  comment calls it "Gmail's own message id → the row it produced", and
  it is filed under *cursor table*, deliberately outside `DATA_TABLES`,
  which is what gets bookkeeping. A sidecar there would model a mapping
  as a fetched entity.

So `download_problems::report_records` writes the row directly, keyed
`record:gmail_messages:<id>`, through the same `replace_prefixed` the
other two reporters use — this run's set replaces the last one's, and a
message that fetches this time stops being a problem without anyone
deleting a row.

That is a second way to say "a record did not fetch", beside
`record_object_error`. The two are for different situations and the
choice between them is not free: use `record_object_error` wherever the
record has a `_bookkeeping` sidecar to stamp, and this only where a
fetch fails before an id in our own keyspace exists.

A cancel that lands mid-backoff arrives as an ordinary error, so both
sites check the stop flag first and write no row — "you stopped this" is
not a fetch failure.

### PR 5 — ~~The downgrade guard can read the run store~~ (void)

**The premise was wrong.** §1.4 said the guard hands a plain-SQLite file
to doltlite's reader and so can never read the run store. Doltlite's
default engine reads a plain SQLite file perfectly well — measured both
ways against the real z14 store:

```sh
datalib-doltlite -readonly .../runs.sqlite "select count(*) from _datalib_meta"   # 6
datalib-doltlite -readonly "file:.../runs.sqlite?doltlite_engine=sqlite" "…"       # 6
```

and a test that plants a newer-versioned plain-SQLite run store is
refused by `inspect_root` with or without an engine change.

What actually happened in z14 is a boot race: the warning is stamped
`13:04:10.727`, two milliseconds after `data root: … (created)`, so the
guard probed the run store in the instant it was being made. That log
holds one server launch, so it is no evidence the warning repeats — the
"every launch" in §1.4 is not supported and should be read as "once, on
a root being created".

What is left is small and cosmetic: do not warn about a store that does
not meaningfully exist yet, or run the guard after the stores are made.

### PR 6 — Stop the log eating itself

- Fix the `/api/processes` storm: either split a step-progress table out
  of `Runs` in [`watch.rs`](../../../datalib/backend/http/src/watch.rs),
  or have `RunLogPanel` refetch only when the attempt set has changed.
  The comment already states the intended rule.
- Make `upserted a batch …` one message with `rows`, `table` and `ms` as
  fields. Grouping by target in the log card becomes useful again.
- Optional, and a judgment call: a floor of about a second on
  `/api/manage/rows` while a run is in flight.

### PR 7 — Bound the two growing stores

- Default the Slack event tape to **off**. It is a debugging tool.
- Give `disk_usage` a retention knob beside the others in
  `[run_history]`, or downsample anything older than a day.

### PR 8 — A step outlives no runner, however the runner died — **done**

This started as "give the worker→runner spawn a process group, so the
`SIGKILL` at the end of PR 1's ladder has something to aim at". That is
not a thing that can work, and the measurement says why. A step under a
runner today:

```
  PID  PPID  PGID  COMMAND
72586 72556 72556  datalib_dag_bin …     runner, in the shell's group
72592 72586 72592  /bin/sh s.sh          step, in its OWN group
72594 72592 72592  sleep 200             grandchild, in the step's group
```

Process groups are flat: a process is in exactly one, and #686 moved
every step into its own so that a signal aimed at a step reaches the
`node` it wrapped. Steps have therefore *left* the runner's group, and
a group at the runner would contain the runner alone.

Worth noticing that #686 removed coverage that used to exist by
accident — before it, a signal to the runner's group did reach the
steps. It was still the right change; the grandchild case was the
common one. But it left a hole that a group cannot close.

What closes it is the other end, and it was already in the tree for
every other long-running process datalib starts. The runner now hands
each step a pipe on stdin with `DATALIB_PARENT_PIPE` set, and
`datalib-step` calls `datalib_parent_watch::exit_with_parent`. A runner
that is SIGKILLed, aborts, or is taken by the OOM killer runs no code
at all, but the kernel still closes its descriptors — so the step reads
EOF and raises on its own group the SIGINT the runner would have sent,
which its existing handler answers by sealing and exiting 130.

This is strictly wider than the ladder's rung 3, which only ever fires
after twenty seconds of a wedged runner. `subprocess.rs` used to say
"a SIGKILL at the runner runs no Rust and leaves the steps behind";
that sentence is now gone.

**The pipe is opt-in: `watches_runner` on the step.** A built-in step
declares it by default, a `command` step defaults to false, and either
can say otherwise. Getting to that took two wrong turns worth recording.

The first cut gave *every* step the pipe and called stdin part of the
protocol. That is backwards: the protection needs the child to *watch*
the pipe, so an arbitrary program gains nothing from holding one — while
a program that reads stdin expecting the immediate EOF `/dev/null` gives
blocks on a pipe nobody writes to. A hung step holding its store open is
the disease, not the cure. Measured: forcing every step onto the pipe
turns `an_arbitrary_step_keeps_dev_null_on_stdin` into a 90-second
timeout on its `cat`.

The second cut keyed the decision off the program's *name*
(`is_datalib_step`). Three things were wrong with that. It is implicit —
wrap or rename the binary and the behaviour changes silently. It put a
decision inside the imperative spawn path when it is a pure function of
the config, against `style.md`. And the test had to copy the probe to a
file called `datalib-step` to reach the path at all: when a test has to
spoof an identity, the code is keyed on the wrong thing.

So the config declares it. `watches_runner` is an assertion about the
program — that it watches stdin for EOF — which is exactly the kind of
thing only the person writing the config can know. It also gives a
custom step a way to ask, which the name-based rule denied it.

Three guards, each watched failing against the behaviour it forbids:
`parent_gone::a_watching_step_exits_when_the_runner_itself_is_sigkilled`
(with the flag off: *"the step outlived the runner that was
SIGKILLed"*), `a_watching_step_is_handed_the_parent_pipe` (the flag's
two halves travel together — `exit_with_parent` refuses the variable
without a pipe), and `an_arbitrary_step_keeps_dev_null_on_stdin` (hangs
if the pipe is handed out unasked).

### Decided, no PR — Gmail's quota ceiling

§1.6 asked whether the ratchet converging to its floor every run was
intended. **It is (decided 2026-09-23).** The cut is cheap, and the
per-run reset is right because the limit is per-user-per-minute and a
fresh run has no memory of the last one. No recovery path, no lowered
default. Leave it alone.

### Decided, no PR — SIGQUIT

#682 gave each step a process group, so a terminal signal no longer
reaches steps directly and the runner has to forward what it cares
about. It forwards SIGINT, SIGTERM and SIGHUP; SIGQUIT is deliberately
left, because Ctrl-\\ asks for a core dump rather than a graceful stop.
It will orphan steps the way SIGHUP did. That is the accepted trade.

### Decided, no PR — this doc is not in the doc map

A plan that has not landed does not go in `AGENTS.md`'s doc map. Add it
when it becomes something a contributor has to read before touching the
cancel path, not before.

### Folded in wherever nearest

- `dag_state.json` recording `"status": ""` / `"attempts": 0` beside
  `"succeeded": true` for in-flight streaming steps.
- Three identical `migrated: stamps to utc + tz_offset` lines at boot on
  a root that was created the same second, none naming which store.
- The double space in `listing  msgs=0 media=0`.

## 3. One thing that held up

The log viewer's source links survive contact with a real store. All 28
distinct `filename` values in the root resolve to real files in the tree
at `787c1a4c`. `sourceOf`'s `bazel-out/<config>/bin/` strip in
[`runLogSource.ts`](../../../datalib/ui/src/components/runLogSource.ts)
is doing real work: 4,892 lines — the whole `datalib_http` library crate
— carry that prefix and would otherwise point at nothing.
