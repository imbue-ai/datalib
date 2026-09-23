# What a cancel leaves behind, and what the log says about it

**Status: PRs 1 and 2 landed (#682, #686, #692, #697); 3 to 7 are
open, and 3, 4 and 5 are not what this doc first said they were —
each says so in its own section. Last read against the tree
2026-09-23.** §1 is what a real data root actually contained — every
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

### PR 3 — A size-limit skip is not a fetch failure

**Bigger than this doc first said, and it needs a decision first.** The
enum variant is one line; getting it to the row is not.

A skip reaches the `problems` table through
`Attachments::add_failed` → `errors: Vec<(String, String)>` →
`record_object_attempt`, which hardcodes `Reason::FetchFailed` and
derives the severity from "was this fetched before?". Nothing on that
path can say *why*. `add_failed` has 20 callers across 8 providers, so
the reason has to arrive some other way — an `add_skipped` beside it,
carrying a `Reason`, is the contained shape. `.errors()` has no callers
outside `blob_cas.rs`, so the tuple can grow.

**The decision, which belongs to whoever owns resume:** a blob skipped
for its size has no `fetched_at_utc`, so the next run tries it again and
skips it again, for ever. That is right if raising
`blob_size_limit_bytes` should pick the file up, and wrong if a skip
should be remembered. The answer decides whether the skip writes
bookkeeping at all, and it is not a detail — it is the difference
between a config change taking effect and not.

The `media` provider has the same shape
(`the_payload_ceiling_leaves_null_and_is_counted`) and is worth checking
in the same pass.

### PR 4 — A Gmail fetch failure reaches the `problems` table

**Costs a schema change, which this doc first missed.** `summary.problems`
is the wrong channel: `DownloadProblem` is keyed on `setting` / `value`
and describes a *configured entry* upstream does not have, not a record
that would not fetch. The right channel is `record_object_error`, as
claude, notion, chatgpt and garmin all use.

Two things stand in the way. The email provider calls it nowhere today,
so this is the first per-record problem it reports. And
`record_object_attempt` writes `{table}_bookkeeping`, which
`gmail_messages` does not have — `emails`, `accounts`, `email_blobs`,
`threads` and `mailboxes` do, and it does not (checked against the live
store, not the schema alone). Scoping the row to `emails` instead is no
escape: at the moment a fetch fails there is no email id yet, which is
the whole reason `Scope::Entity` takes the raw id.

So it needs a bookkeeping sidecar for `gmail_messages` — a raw-store
shape change, a minor version bump, and a line in the commit message
saying what it invalidates (`schema_migrations.md`).

Still check the stop flag first and write no row when the cause was a
cancel, per §1.3. This is one concrete instance of the per-provider
fetch tail that [`problem_visibility.md`](problem_visibility.md) §3
lists as still open.

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
