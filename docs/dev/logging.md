# Logging — one store, every process, and how to add a line

Reference for the tree as of #626. The design record, with the
arguments for each decision, is
[`plans/completed/logs_and_metrics.md`](plans/completed/logs_and_metrics.md);
the step-side details (pipes, envelopes, flushing, the error tail) are
in [`step_protocol.md`](step_protocol.md) § "stderr: logging". This
page is the map.

## One store

Everything any datalib process says lands in one file,
`<data_root>/system/runs/runs.sqlite` — plain SQLite in WAL mode, so
`sqlite3` opens it and two writers share it through SQLite's own
locking. The runner writes it during a run; `datalib-http` writes it
for the life of the server. The tables:

| table | one row per |
|---|---|
| `processes` | a process that took part (below) |
| `runs` | a run of the runner |
| `step_runs` | a step in a run: state, attempt, error, message |
| `log` | a line |
| `metrics`, `metric_samples` | a step's numbers — the newest value, and a sparse timeseries |

Every stamp is UTC in a `*_utc` column with the offset the clock was
in beside it (`tz_offset`); text order is instant order. A line keeps
its own clock when it had one (a tracing envelope's `timestamp`, a
page's `at`), else the moment the writer saw it.

Retention is `[run_history]` in `config.toml`
([`configs/dag_example.toml`](../../configs/dag_example.toml)):
`max_runs` / `max_age_days` for runs and everything that belongs to
one, `process_log_days` / `process_log_lines` for the lines outside
any run — the server's and the pages'. The store is not load-bearing:
one that will not open is remade, and a schema bump remakes it.

## Every line has an author

A log line names the **process** that wrote it (`log.process_id` →
`processes`), and the process row carries what a line should not
repeat: which program it was, when it started and ended, how it ended
when something saw, and the commit it was built from — what a line's
`filename` and `line_number` are relative to.

| `process` | what | who records it | ends when |
|---|---|---|---|
| `dag` | a run of the runner | itself | it records nothing; the run row closes |
| `step` | one attempt of one step | the runner, at spawn and at `wait(2)` | `exit_code` or `signal` |
| `http` | a launch of the server | itself | it cannot see its own end |
| `ui` | one load of the app in one browser tab | the server, on the page's behalf | the page says so on `pagehide` |

A line also keeps its **subject** — `run_id`, `step`, `attempt` — which
is not the same thing: the runner's line "step X failed" is authored by
the runner and about step X.

The commit belongs to the process, not the store, because lines from
different builds sit in one file — the server restarts between
versions. It comes from `datalib_runtime::build_id::git_hash`: the
`DATALIB_GIT_HASH` environment variable (the dev launchers set it from
the checkout), else a `git-hash` file beside the binaries (the release
tarball and the .app carry one); a binary that can say neither records
nothing, and the log's source links point at `main` instead of a
commit. The link is built when the line is shown, never stored: the
store keeps the path rustc saw and the commit, and the UI
(`runLogSource.ts`) makes the URL. Nothing is compiled in — a build
stamp would rebuild everything downstream on every commit. A step attempt running the built-in step
program shares the runner's commit; a custom command has none; a page
has the server's, since the bundle is embedded in the binary.

## Where a line comes from

| you are writing | do this | it becomes |
|---|---|---|
| Rust in the runner or the server | `tracing::info!(target: "…", key = value, "the sentence")` | a row with `target`, `msg`, and the keys as a JSON object in `fields`; `filename` / `line_number` added ([`runs/src/tracing_layer.rs`](../../datalib/backend/runs/src/tracing_layer.rs)) |
| Rust in a built-in step (`datalib-step`) | the same `tracing` call | a JSON envelope on the step's stderr, which the runner unwraps into the same columns; the line's own timestamp wins |
| a custom step, any language | print a line on stderr (or a non-event line on stdout) | an `info` row with `stream` set; the last lines before a non-zero exit also become the step's error |
| the server, per request | nothing — [`http/src/request_log.rs`](../../datalib/backend/http/src/request_log.rs) does it | `target:http.request`: method, path, query, status, `ms`, `bytes`, and the `page` that asked |
| the UI | `track("name", { …fields }, { level, msg })` from [`ui/src/telemetry.ts`](../../datalib/ui/src/telemetry.ts) | `target:ui.name` under the page's own process, with the page's clock; batched, `keepalive`, never throws |

Adding a UI event is one word in the `PageEventName` union and the
call; the server files any word. Uncaught exceptions and route changes
are already tracked — see the union for what is.

Levels are `trace` … `error`. A process starts from
`datalib_log_filter::DEFAULT_LOG_FILTER` (our crates at `debug`, the
noisy libraries at `warn`) unless `RUST_LOG` says otherwise, and the
store keeps every level it is handed — `debug` is where a doltlite
commit or a batch of rows goes. The log card opens at
`min_level:info`, so `debug` is there when asked for and not otherwise.

## What is not a log line

- **Anything about a record** — a fetch that failed, a render that
  could not project a field — goes through `problems`
  ([`plans/problem_visibility.md`](plans/problem_visibility.md)), which
  travels with the data and reaches the Manage counts and the document
  banner. A `warn!` reaches nobody who is not reading the log.
- **A number** — rows written, requests made, queue depth — is a
  `metric` event, not a sentence with a number in it. The Activity
  column and the rates come from `metric_samples`.
- **A secret.** The request log drops `?token=`; a line you write must
  not carry a credential either.

## Reading it

**In the app.** The log is a card, `logView({ run, step, launch })`,
so it sits in the URL like any other. Manage opens it: **Server log**
on the header (this server's launch, with earlier launches, the pages
of the app and the runs in the picker), **Show log** on a step's menu
(its newest attempt), a double-click on a Failed row. Selecting a line
opens `logLineView(seq)` beside it: the whole message, the fields as a
tree with copy and keep / exclude, the source link at the process's
commit, both clocks. The search bar takes the grammar every grid
shares: the keys are the columns — `run`, `process`, `step`, `level`,
`stream`, `target`, `thread`, `msg` — plus `min_level:warn` (this
level and above) and `commit:0fc29cb` (prefix). Right-click a cell to
keep or exclude its value; drag a column header into the bar to group.
The card tails while what it shows may still be writing.

**Over the API** (`Authorization: Bearer <token>`, the token being
`<root>/system/api-token`):

```
GET /api/processes?run=&process=&limit=       the authors, newest first
GET /api/runs                                 recent runs
GET /api/runs/{run}/steps                     step_runs + current metrics
GET /api/log?run=&process=&step=&attempt=&q=&after_seq=&limit=
GET /api/log/{seq}                            one line, with its process row
```

`after_seq` is the tail cursor: remember the last `seq`, ask again on
the SSE `table_changed: log` frame.

**From a shell**, since it is plain SQLite:

```sh
sqlite3 <root>/system/runs/runs.sqlite \
  "SELECT l.ts_utc, p.process, l.level, l.target, l.msg
     FROM log l LEFT JOIN processes p USING (process_id)
    ORDER BY l.seq DESC LIMIT 50"
```

## Rules

- **A response that is the log must not write the log.** The card
  refetches on every `log` frame; a request line for `/api/log` would
  move the store, which is the frame, which is the next request.
  `request_log.rs` skips those two routes; a new endpoint that serves
  log rows joins the list.
- **A step flushes per line.** Arrival order is the log's order, and a
  block-buffered stdout hands the runner its lines in 4KB lumps,
  minutes late. The runner sets `PYTHONUNBUFFERED=1`; anything else is
  the step's job.
- **Fields, not interpolation.** `job = %id, "claim failed"` is a
  `fields.job` anyone can filter and group on; `"claim failed for
  {id}"` is a sentence.
- **A stamp you mint is UTC + offset**, never a bare local time
  (`IsoOffsetTimestamp::now_local().to_utc_and_offset()` in Rust,
  `nowIso()` in the UI).
