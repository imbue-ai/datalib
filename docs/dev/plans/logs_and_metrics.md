# Logs and metrics: one store, written by the runner

**Status: agreed plan (2026-09-11), being built.** [Order of work](#order-of-work)
is the checklist; update it as slices land, and treat anything it still
lists as unbuilt. Per [`AGENTS.md`](../../../AGENTS.md), where this file
says "today" that was checked against `5f589a59`; where it says "will",
nothing exists yet.

This supersedes §3 ("Pipeline state as a table") and §4 ("Run logs,
beside the data") of [`data_centric_ui.md`](data_centric_ui.md). That
proposal put each step's log in a file under the step's own tree; the
decision here is one file per data root, for the reasons in
[§"One file, not one per step"](#one-file-not-one-per-step). Issues:
[#161](https://github.com/imbue-ai/datalib/issues/161) (the progress bar
is bizarre), [#164](https://github.com/imbue-ai/datalib/issues/164)
(follow a step's log as it runs),
[#136](https://github.com/imbue-ai/datalib/issues/136) (stall detection).

## The goal

Three things the Manage screen should be able to show for every step,
live, and for past runs:

1. **Its log**, as rows in a grid — timestamped, per step, sortable and
   filterable — appended to as the step runs.
2. **What it has done so far**, as numbers: rows written, requests made,
   bytes fetched, per table where that makes sense. Not a percentage.
3. **How much is queued in front of it.** A step rarely knows its total
   up front, especially once chunks stream through the graph; it usually
   does know what is waiting. Per Brendan Gregg's
   [USE method](https://www.brendangregg.com/usemethod.html), the three
   questions per resource are utilization, saturation and errors, and
   for a step those are *is it running*, *how deep is its queue*, and
   *how many errors has it logged*.

And the constraint that shapes the whole design: **a step stays a plain
command.** A shell script is a valid step and cannot write SQLite. So
the capture happens in the framework that runs it.

## What is there today: three paths carrying the same facts

| path | writer | reader | carries |
|---|---|---|---|
| `system/progress.sqlite` — plain SQLite (`doltlite_engine=sqlite`), WAL | `datalib-dag`, through `dag/src/progress_bus.rs` | `GET /api/dag`, drawn on Manager2's per-step row | one row per step: state, `done`/`total`/`msg`; **wiped at the start of every run** |
| `sync_jobs.progress_msg`, a JSON "task board" | `datalib-http`'s worker, re-parsing the runner's NDJSON in `worker.rs::TaskBoard` | SSE `job` frames → `StepProgress.vue` | one cell per step, running ones flashing — the bar #161 is about |
| `system/job-logs/<job>.log` | the same worker, teeing both of the runner's pipes | `GET /api/sync/jobs/{id}/log`, then `ui/src/config/stepLog.ts` filters to one step and unwraps tracing JSON **in the browser**, fetching up to eight whole files to find the step | raw NDJSON lines |

The first is the right shape and the other two are what this plan
deletes. Two things come with the deletion for free:

- A `datalib-dag` run started from a terminal becomes visible. Today it
  has no job row, so no log — the Manage screen's log panel says so in
  its own footer.
- The tail nudge already exists. `http/src/watch.rs` publishes a
  `dag_changed` frame on the SSE stream whenever the bus's `-wal` file
  moves; a client that refetches "rows after `seq` N" on that frame is
  tailing.

Also there and not reaching the UI as data:
`etl/src/download_metrics.rs` already counts `api_requests` and
`rows_upserted` per table for every download — and then formats them
into the progress *message* (`api=12 rows[messages=340]`).

## The design

### One store, one writer

`system/runs.sqlite` replaces `system/progress.sqlite`. Same engine
choice (plain SQLite through doltlite's `doltlite_engine=sqlite` URI
parameter, WAL, `synchronous=Off`), same crate (`datalib_progress`,
renamed `datalib_runs`), same single writer: the runner. The runner is
the one process that sees every event from every step, whatever the
step is written in.

It is **not wiped per run**. Rows carry `run_id`, and a run's rows
accumulate until a retention rule removes them, so "what did this step
log last Tuesday" is an ordinary query and the log panel stops walking
eight files.

Tables:

```
runs           run_id, started_at, finished_at
step_runs      run_id, step, state, attempt, started_at, finished_at,
               error, msg, updated_at
log            seq (rowid), run_id, step, ts, level, target, msg, fields
metrics        run_id, step, name, labels, value, updated_at
metric_samples run_id, step, name, labels, ts, value
```

`step_runs` is today's `step_progress` with history and without
`done`/`total` (those become metrics, below). `log.fields` is a JSON
object of whatever structured fields the line carried beyond its
message; `target` is the tracing target when the line was tracing
JSON. `metric_samples` is a timeseries in the shape of
`usage.doltlite_db`: appended only when a value changed and at most
every few seconds per series, so a rate (rows/s) is a query and #136's
"busy but not advancing" is `value` flat while `log` rows keep coming.

**Retention is config.** A `[run_history]` table at the top of
`config.toml` with `max_runs` and `max_age_days`; the runner prunes at
the start of each run. SQLite reuses freed pages, so the file stays
bounded by the rule without a `VACUUM`.

**A file that will not open is deleted and remade.** `synchronous=Off`
means an OS crash can leave the file unreadable. Nothing in it is
load-bearing, so the honest response is a warning and a fresh file, not
a run that refuses to start.

### One file, not one per step

`data_centric_ui.md` §4 wanted `<step>/run_logs.sqlite`, so a source's
history travels with its data and a step run by hand still logs. The
arguments for one file won:

- The writer is the runner either way. A step run by hand outside the
  runner is not a case worth a second write path; the runner *is* how a
  step is run.
- A run's log spans steps, and "this step only" is a `WHERE`. Per-step
  files make the run view a fan-out across N files, each with its own
  WAL sidecar to watch.
- Deleting a source's directory is not the moment its log should
  vanish; the log of the run that removed it is the one you want.

### Progress becomes metrics

One new event replaces `progress_length` / `progress_inc`:

```json
{"event":"metric","step":"slack/ingest","name":"rows_upserted","labels":{"table":"slack_messages"},"value":1234}
```

- **Absolute values, never deltas.** The bus coalesces at 200ms, and
  coalescing deltas loses work; coalescing positions is lossless.
  (`progress_bus.rs` already keeps an accumulator for exactly this
  reason; it stays, for the sugar below and nothing else.)
- **A gauge is just a value that can go down.** `queued` is the one that
  matters; nothing distinguishes it on the wire.
- **Labels are a small map**, canonicalized to one string for the key.
  `rows_upserted{table=…}` is the case that needs them.

`DownloadMetrics` emits these directly instead of composing a suffix.
`progress_message` stays: a step's own words ("conversations.list") are
a phase, not a number, and they land in `step_runs.msg`.

**`progress_length` / `progress_inc` stay as sugar** for a third-party
step, and the runner translates: `done` is the accumulated increments,
`queued` is `total - done` when a total was given. `step_protocol.md`
documents `metric` as the primary form.

### Queue depth, without opening a store

Per USE, each step's row shows:

- **U** — running, and since when.
- **S** — `queued`. Two sources:
  - *Inside a step*: the step's own gauge. A download that has listed
    its channels and is fetching them knows the count; a render that has
    scanned its cursor knows how many rows are ahead.
  - *Between steps*: the `checkpoint` event gains a `rows` field — the
    producer knows how many rows it just sealed. The runner keeps, per
    consumer, the sum of rows sealed past the producer version that
    consumer last consumed, and publishes it as the consumer's `queued`.
    No store is opened to measure it, which matters: a reader on a live
    store is not free (AGENTS.md, "A reader can be the peer").
- **E** — `log` rows at `warn`/`error` this run, plus attempts beyond
  the first.

This is also how the streaming dispatch becomes observable. A
consumer's `queued` that never drains while its producer keeps sealing
is the symptom; today there is no way to see it.

### Logs are unwrapped at the source

`subprocess.rs` already parses each stderr line as JSON to find its
level. It will parse the whole tracing envelope — `fields.message` (or
`fields.event`), `target`, the leftover fields — into a richer
`Event::Log`, so the NDJSON stream and the store both carry columns,
and `stepLog.ts`'s browser-side unwrapping is deleted.

### The worker shrinks

`datalib-http`'s worker spawns the runner with `--now <job id>`, so the
job row names its run, and waits. `TaskBoard`, `pump`, the log tee,
`progress_msg`-as-JSON, `StepProgress.vue`, `ui/src/sync/progress.ts`
and `stepLog.ts` all go. New endpoints, all reads of the one file:

```
GET /api/runs                              recent runs
GET /api/runs/{run}/steps                  step_runs + current metrics
GET /api/runs/{run}/log?step=&after_seq=   the tail, since seq
GET /api/runs/{run}/metrics?step=          samples, for rates
```

The SSE `dag_changed` frame is the nudge for all four.

### The UI

Manager2's per-step row gets three cells in place of one bar: the
metrics (compact `name=value` chips, with a rate where samples exist),
`queued`, and the error count. Double-click still opens the log — now an
AG Grid over `/api/runs/{run}/log`, appending on each nudge, with the
step filter pre-set and removable (so "the whole run" is one click
rather than a different panel).

## Order of work

Each slice is one PR that leaves the tree green.

1. ~~**The store and the runner**~~ **Done.** `datalib_runs`
   (`datalib/backend/runs/`), the five tables, `[run_history]`
   retention, the `metric` event, `Log` carrying `target` and `fields`
   (the runner unwraps tracing envelopes in `subprocess.rs`),
   `DownloadMetrics` publishing through the step's `Progress`, the
   sugar translated in `dag/src/runs_sink.rs`. `GET /api/dag` serves
   `progress.metrics` as a map; the Python e2e test reads the new
   tables with stdlib sqlite3. Found on the way: `DownloadMetrics`'s
   `api=… rows[…]` suffix had no caller — the counters were never
   shown anywhere before this.
1b. ~~**Run ids and log columns**~~ **Done.** The run id is a UUID v7
   the runner mints (or `--run-id`; the worker passes its job id), in
   `DATALIB_DAG_RUN_ID` beside `DATALIB_DAG_ATTEMPT` so a step can stamp
   what it writes. `log` rows carry `attempt`, `stream` (which pipe),
   `thread`, and the line's own timestamp when it had one; `filename`
   and `line_number` stay in `fields`. `PYTHONUNBUFFERED=1` on every
   child. `PRAGMA user_version` gates the schema: a store from another
   version is remade.
2. ~~**Delete the other two paths and repoint the UI**~~ **Done.** The
   worker spawns, waits and records (`TaskBoard`, the log tee and the
   `progress_msg` board are gone; only a bounded tail of the runner's
   own lines survives, into the job's error). `GET /api/runs`,
   `/api/runs/{run}/steps`, `/api/runs/{run}/log?step=&after_seq=`
   replace `/api/sync/jobs/{id}/log`. `LastRun` records its run id.
   Manager2 reads everything from `/api/dag` on `dag_changed` (the
   pushed-board overlay in `pipelineStatus.ts` is gone; `stepForRun`
   is what is left), double-click opens `RunLogPanel.vue` — an AG Grid
   over the run log, tailing by `seq` while the run is live — and an
   **Activity** column shows `queued`, every metric, and the warn/error
   count. `StepProgress.vue` is one bar; `sync/progress.ts` and
   `stepLog.ts` are deleted. The old `/sources` tab keeps working on
   the new endpoints. Closes #161 and #164.
3. **Queue depth between steps** — `rows` on `checkpoint`, the
   runner's per-consumer sum, `queued` published for consumers.
4. **Rates and flatlines** — `metric_samples` drawn as rates; a step
   whose `metrics` stopped moving while `log` did not, flagged. Closes
   the "progress-flatline" item of #136.

## Open questions

- **Joining a raw store's `sync_runs` row to the run.** Steps now
  receive `DATALIB_DAG_RUN_ID`; nothing stamps it yet. A `dag_run_id`
  column on `sync_runs` (and on the render cursor) is the obvious next
  step, and would let the Manage screen link a log line to the commit
  it produced.
- **Bytes.** `DownloadMetrics` counts requests and rows; nothing counts
  bytes fetched or bytes written. The HTTP chokepoint sees the response
  body length, so `bytes_fetched` is a one-line addition there. Bytes
  written per store is what `usage.doltlite_db` already samples, from
  the outside; whether to also count it from the inside is not decided.
- **Whether `metric_samples` wants a cap of its own.** At one sample per
  changed series per five seconds a busy download writes ~700 rows an
  hour per series; the run retention bounds it, but a run that lasts a
  day with twenty series is a few hundred thousand rows. Measure before
  deciding.
