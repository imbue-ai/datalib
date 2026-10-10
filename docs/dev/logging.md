# Logging — one store, every process, and how to add a line

The design record, with the arguments for each decision, is
[`plans/completed/logs_and_metrics.md`](plans/completed/logs_and_metrics.md);
the step-side details (pipes, envelopes, flushing, the error tail) are
in [`step_protocol.md`](step_protocol.md) § "stderr: logging". This
page is the map.

## One store

Everything any datalib process says lands in one file,
`<data_root>/system/runs/runs.sqlite` — plain SQLite in rollback-journal mode, so
`sqlite3` opens it and two writers share it through SQLite's own
locking. The runner writes it during a run; `datalib-http` writes it
for the life of the server. The tables:

| table | one row per |
|---|---|
| `processes` | a process that took part (below) |
| `runs` | a run of the runner |
| `step_runs` | a step in a run: state, attempt, error, message. While a run is live every step of the config is here, `pending` until reached; when it ends, the ones no request reached go, so a closed run holds the steps it ran. A run whose runner died keeps them, as `stopped`: nothing settled them, so any might have been about to run |
| `log` | a line |
| `metrics`, `metric_samples` | a step's numbers — the newest value, and a sparse timeseries |
| `store_changes` | a part of the store a reader can depend on (`runs`, `step_runs`, `metrics`, a run's lines, the server's lines), with a counter each write bumps |

Every stamp is UTC in a `*_utc` column with the offset the clock was
in beside it (`tz_offset`); text order is instant order. A line keeps
its own clock when it had one (a tracing envelope's `timestamp`, a
page's `at`), else the moment the writer saw it.

**Two writers on one file is the design, and it is measured.** SQLite
serializes writers with a lock on the file rather than letting them
overwrite each other: a writer that cannot take the lock waits ten
seconds and is then handed an error. A batch that fails that way is one
transaction that wrote nothing, so the writer offers the same batch once
more before giving up — and when it does give up it says how many lines
went with it, because a silent loss here is what makes anyone distrust
the store. Deciding what to *do* with the file is exclusive between
processes (`runs.sqlite.open-lock`): two processes that both found an
old store would each empty it, the second emptying what the first had
already remade. Remaking empties the file in place and never deletes
it: a reader that opened the old file would stay on the deleted one,
where no writer holds a lock, take the new file's journal for a crashed
writer's and delete it — and the new writer's commit would fail. So
the remake is SQLite's own reset, under the file's lock like any write
(`open_or_recreate` in `datalib/backend/runs/src/store.rs`).
`runs_two_process_test` runs four writers
at once — on a fresh root, and on one this build has to remake — and
checks that every line published reaches the store.

One thing this rests on: SQLite's file locking is not dependable on a
network or file-syncing filesystem. **A data root belongs on local
disk**, not on an NFS or SMB mount or inside a Dropbox folder.

Retention is `[run_history]` in `config.toml`
([`configs/dag_example.toml`](../../configs/dag_example.toml)):
`max_runs` / `max_age_days` for runs and everything that belongs to
one, `process_log_days` / `process_log_lines` for the lines outside
any run — the server's and the pages'. The store is not load-bearing:
one that will not open is emptied and remade, and a schema bump does
the same. The old file is copied first to `runs.bak_<UTC stamp>.sqlite`
beside it, so its lines can still be read with `sqlite3`; nothing
deletes those copies, so remove them by hand when you are done with
them. A copy that fails is an ERROR and costs the old lines, never the
new store. A writer that cannot open it at all is refused at the start,
with an ERROR saying why, and the caller says nothing will be recorded.

## Every line has an author

A log line names the **process** that wrote it (`log.process_id` →
`processes`), and the process row carries what a line should not
repeat: which program it was, when it started and ended, how it ended
when something saw, and the commit it was built from — what a line's
`filename` and `line_number` are relative to.

| `process` | what | who records it | ends when |
|---|---|---|---|
| `dag` | a run of the runner | itself | it records nothing; the run row closes |
| `step` | one attempt of one step — or one pass of a streaming consumer, which is spawned once per producer checkpoint; every pass is its own process, all under `attempt 1` | the runner, at spawn and at `wait(2)` | `exit_code` or `signal` |
| `http` | a launch of the server, and what its applets said | itself | it cannot see its own end |
| `ui` | one load of the app in one browser tab | the server, on the page's behalf | the page says so on `pagehide` |

A line also keeps its **subject** — `run_id`, `step`, `attempt` — which
is not the same thing: the runner's line "step X failed" is authored by
the runner and about step X. A line about a step also carries the step's
`group_id`, the `[[groups]]` entry it is filed under, as the runner's
plan said, so a source's lines can be read together whatever step wrote
them.

The commit belongs to the process, not the store, because lines from
different builds sit in one file — the server restarts between
versions. It comes from `datalib_runtime::build_id::git_hash`: the
`DATALIB_GIT_HASH` environment variable (the dev launchers set it from
the checkout), else a `git-hash` file beside the binaries — the
release tarball and the .app carry one, and so does
`bazelisk build //datalib/backend:bin`, so a `datalib-http` run
straight out of `bazel-bin/datalib/backend/bin/` has it too. The
process says which at boot (`build commit read from …`); one that has
neither warns instead and records nothing, and the log's source links
point at `main` instead of a commit. The link is built when the line
is shown, never stored: the store keeps the path rustc saw and the
commit, and the UI (`runLogSource.ts`) makes the URL. Nothing is
compiled into a binary — a rustc stamp would rebuild everything
downstream on every commit; the staged file is one stamped genrule
(`.bazelrc` §stamping). A step attempt running the built-in step
program shares the runner's commit; a custom command has none; a page
has the server's, since the bundle is embedded in the binary.

## Where a line comes from

| you are writing | do this | it becomes |
|---|---|---|
| Rust in the runner or the server | `tracing::info!(target: "…", key = value, "the sentence")` | a row with `target`, `msg`, and the keys as a JSON object in `fields`; `filename` / `line_number` added ([`runs/src/tracing_layer.rs`](../../datalib/backend/runs/src/tracing_layer.rs)) |
| the runner, about a step — a checkpoint sealed, why it ended, a hint | nothing — the runner writes these itself | `target:datalib_dag::runner` under the step, with the runner as author |
| Rust in a built-in step (`datalib-step`) | the same `tracing` call | a JSON envelope on the step's stderr, which the runner unwraps into the same columns; the line's own timestamp wins |
| Rust in an applet (`datalib-applet`) | the same `tracing` call | a JSON envelope on the applet's stderr, which the gateway logs again as the server's line at the envelope's level: `target:datalib_http::applets`, with `applet` naming it and the applet's own target and fields in `applet_target` and `applet_fields`. The row's `filename` / `line_number` are the gateway's; the applet's are inside `applet_fields` ([`applets.md`](applets.md)) |
| a custom step, any language | print a line on stderr (or a non-event line on stdout) | an `info` row with `stream` set; the last lines before a non-zero exit also become the step's error |
| the server, per request | nothing — [`http/src/request_log.rs`](../../datalib/backend/http/src/request_log.rs) does it | `target:http.request`: method, path and query (with what a person typed taken out: § "What a line may carry"), status, `ms`, `bytes`, the `page` that asked, and the `card` and `card_type` when a card asked (`ui/src/cards/cardScope.ts`; `ui.card_open` says what source that card ran); `debug` for a live refetch that succeeded (below), `info` otherwise |
| the UI | `track("name", { …fields }, { level, msg })` from [`ui/src/telemetry.ts`](../../datalib/ui/src/telemetry.ts) | `target:ui.name` under the page's own process, with the page's clock; batched, `keepalive`, never throws |

Adding a UI event is one word in the `PageEventName` union and the
call; the server files any word. Uncaught exceptions, route changes
and every toast shown (`target:ui.toast`, its text as the line, at its
level) are already tracked — see the union for what is.

Levels are `trace` … `error`. The level a root logs at is
`log_level` at the top of its `config.toml` — `trace` when it does
not say, while the pipeline is being debugged — and it applies to
datalib's own crates in every process of that root: the server reads
it at launch (a change takes a restart), the runner per run, and each
step gets the resulting filter as `RUST_LOG`. Third-party crates
follow it down to `info` and no further, and the noisy ones stay at
`warn` (`datalib_log_filter`); the server's own `http.*` targets count
as datalib's. A `RUST_LOG` set where a process was
started wins over all of that. The store keeps every level it is
handed — `debug` is where a doltlite commit or a batch of rows goes —
and the log card opens at `min_level:info`, so the lower levels are
there when asked for and not otherwise. Little emits `trace` today:
the few `trace!` lines in the tree are a step's progress ticks
(`etl/src/progress.rs`), so a log with none is the normal case, not a
sign the level is lost. The server's one `debug` line in ordinary
service is a live refetch: a request a page made on a `root` frame,
which while a sync runs is about one a second for each Manage card
on screen.

## What is not a log line

- **Anything about a record** — a fetch that failed, a render that
  could not project a field — goes through `problems`
  ([`plans/problem_visibility.md`](plans/problem_visibility.md)), which
  travels with the data and reaches the Manage counts and the document
  banner. A `warn!` reaches nobody who is not reading the log, so do
  not write one beside a problem: the step logs what it stored, once,
  at its end — one `problems_recorded` line per kind of problem (stage,
  reason, rule, field) with its `count`, at its loudest row's severity
  (`error`, `warn`, or `debug` for a finding), and never a row's
  `sample` or key, which hold the record's own contents. A writer that
  stores a row it made calls `datalib_problems::note_recorded`; one that
  copies another step's rows does not. `ingested_tng_test` checks the
  counts against the rows.
- **A number** — rows written, requests made, queue depth — is a
  `metric` event, not a sentence with a number in it. The Manage
  screen's queue and ETA (in its Status column) and the sync dashboard's charts come from
  `metric_samples`.

## What a line may carry

Logs get pasted into public issues and handed whole to an agent
diagnosing a failed sync, so a line must make sense to a stranger and
tell them nothing about the person or their data.

- **Never a secret**: a token, a password, a cookie, an API key.
- **Never private data**: what a record says (message text, a subject
  or title, a person's name, an email address, a phone number, a street
  address), a URL a record holds, the name of a file or folder in a
  person's mirror, an account's name or email, and anything a person
  typed, such as a search.
- **Fine**: ids (a uuid, an upstream row id), counts, sizes, timings,
  step ids, source ids, table and column names, error kinds, and paths
  to datalib's own stores.

So a line about a record names it by id, and what it says goes through
`problems` (above). An error chain is the easy way to break this: an
`anyhow` context or an upstream error that quotes the value it could
not parse carries that value into the line.

Two things are taken out where lines enter the store, because they are
cheap to spot and nobody should have to remember them:

- **The request log keeps what was asked, not what was typed.** A
  query string keeps its keys and only the values of the keys in
  `KEPT_VALUES` (`http/src/request_log.rs`) — counts, cursors, ids,
  column names — so `q=<redacted>&limit=50`. A path the app routes
  itself keeps only its cards' names, `GET /gridView`, because the
  card's arguments are where a grid's search lives. `?token=` is
  dropped whole.
- **The home directory is `~`** in every line's message and fields and
  in a step's error, so a path names no user (`runs/src/redact.rs`).

Neither reads what a line means: a subject in a `warn!` field stays a
subject. `//datalib/backend/http:request_log_test` sends a search
through the server and fails if what was typed reaches the store. The
log lines known to break the rule are listed in
[`audits/2026-10-10_log_privacy.md`](audits/2026-10-10_log_privacy.md).

## Reading it

**In the app.** The log is a card, `logView({ run, step, launch })`,
so it sits in the URL like any other. **Logs** in the status bar opens
it on everything (the picker holds the runs, this server's launch and
earlier ones, and the pages of the app); Manage opens it through
**Show step log** on a step's menu (its newest attempt) and a double-click
on a Failed row. Selecting a line
opens `logLineView(seq)` beside it: the whole message, the fields as a
tree with copy and keep / exclude, the source link at the process's
commit, both clocks. The grid shows Time, Step, Level, Stream,
Source, Message and Fields; the columns that would say the same thing on line
after line of one process's log — run, process, commit, thread, target
— start hidden, as does Group, which the Step chip already names
("Slack · Download"), and the grid menu at the top right puts any of
them back. The search bar takes the grammar every grid
shares: the keys are the columns — `run`, `process`, `step`, `group`,
`level`, `stream`, `target`, `thread`, `msg` — plus `process_id` and `attempt`,
`min_level:warn` (this level and above) and `commit:0fc29cb` (prefix).
The pickers above the grid are views of the query: picking a run, a
launch or a step's attempt writes `run:`, `process_id:` or `step:` and
`attempt:` into it, and clearing the query shows the whole store. Right-click a cell to
keep or exclude its value; drag a column header into the bar to group.
The card tails while what it shows may still be writing.

**Over the API** (`Authorization: Bearer <token>`, the token being
`<root>/system/api-token`):

```
GET /api/processes?run=&process=&limit=       the authors, newest first
GET /api/runs                                 recent runs
GET /api/runs/{run}/steps                     step_runs + current metrics
GET /api/runs/{run}/log?step=&after_seq=&limit=  one run's lines, oldest first
GET /api/log?q=&limit=&after_seq=|before_seq=  lines, oldest first
GET /api/log/{seq}                            one line, with its process row
```

On `/api/log`, `q` is the whole of what is asked: the panel's pickers
write what they pick into it as `run:`, `process_id:`, `step:` and
`attempt:`, and any parameter but `q`, `limit`, `after_seq` and
`before_seq` is refused. With no cursor the answer is
the newest `limit` lines; `before_seq` pages back from the oldest one
held, and `after_seq` is the tail cursor: remember the last `seq`, ask
again on the SSE `table_changed: log` frame.

**From a shell**, since it is plain SQLite:

```sh
sqlite3 <root>/system/runs/runs.sqlite \
  "SELECT l.ts_utc, p.process, l.level, l.target, l.msg
     FROM log l LEFT JOIN processes p USING (process_id)
    ORDER BY l.seq DESC LIMIT 50"
```

### From outside the app

`GET /metrics` serves the numbers in Prometheus's text exposition
format, the one every metrics tool reads: Prometheus itself, Grafana's
agent, an OpenTelemetry collector's Prometheus receiver. It is behind
the API token like every route, so a scrape sends it as a bearer
token:

```yaml
scrape_configs:
  - job_name: datalib
    static_configs: [{ targets: ["127.0.0.1:8731"] }]
    authorization: { credentials_file: <root>/system/api-token }
```

What it serves (`http/src/prometheus.rs`):

- **`datalib_step_<name>`**: every series a step the config declares
  has reported, its newest value from the last run it reported in,
  labelled `step`, `group` and the series' own labels. The type comes
  from the name: one ending in `_total` is a counter, anything else a
  gauge (`step_protocol.md` §"stdout: the event protocol" has the naming
  rules). A step's counters start again from zero each run, which a
  scraper reads as a counter reset.
- **`datalib_step_state{state=…}`**: 1 for the state the sync loop last
  put the step in, 0 for the others — `running`, `failed`, `off`, …
- **`datalib_step_last_success_timestamp_seconds`**: for an alert on a
  source that has not synced in a day.
- **`datalib_tree_bytes{tree=…}`** and **`datalib_root_bytes`**: what
  the usage sampler last measured.

`//tests/fixtures:metrics_export_e2e_test` scrapes a real server and
parses the answer with `prometheus_client`, the Prometheus project's
own parser, so a line a scraper would refuse fails CI.

Its history starts when something begins scraping; the app's own views
read the run store, which has every run it keeps. Labels are step and
group ids and what a step labels its series with (a table name, a
producer), never a record's contents. Log lines do not go out this way.

## Rules

- **A response that is the log must not write the log.** The card
  refetches on every `log` frame; a request line for `/api/log` would
  move the store, which is the frame, which is the next request.
  `request_log.rs` skips those two routes; a new endpoint that serves
  log rows joins the list.
- **A loop that gets past that rule is counted.** A `root` frame
  that only the server's own request lines moved carries `chain`, one
  more than the longest chain among those lines. A fetch the page makes
  while handling any frame sends `X-Datalib-Cause` — the frame's chain,
  or 0 — and the request's line stores a nonzero one as `fields.chain`;
  the header is also what makes the line a live refetch, and `debug`. At 5, and again at 50,
  500 and so on, the server writes a `warn` with target `http.loop`
  naming the endpoint and the page. Search `target:http.loop` to find
  one. Only a fetch started synchronously inside the frame's handler
  carries the header, so a refetch deferred by a timer is not counted
  (`loop_guard.rs`).
- **A card off screen is not told.** A card in a hidden tab or layout
  stays mounted, so a card that subscribes with
  `subscribeLive(handlers, { onScreen: el })` (`ui/src/live.ts`) has its
  frames held while `el` is off screen and delivered, once each, when
  it is back. Every card that refetches on a frame passes its root
  element.
- **A step flushes per line** ([`step_protocol.md`](step_protocol.md)
  § "stderr: logging").
- **Fields, not interpolation.** `job = %id, "claim failed"` is a
  `fields.job` anyone can filter and group on; `"claim failed for
  {id}"` is a sentence. And a sentence, always: a line whose only
  message is an `event = "name"` reads as an identifier in the card and
  is found by neither a `msg:` search for words nor one for the name.
  Keep `event` as a field beside the sentence where a name helps.
- **A field is a value, not a `Debug` rendering.** `?opt` puts
  `Some(Origin)` in the store, which nothing can filter on; write
  `opt.map(Reach::as_str)`, `.as_deref()`, or `%list.join(",")`.
  `ingested_tng_test` reads the fixture's store after every run and
  fails on a `Some(`, a `None`, a `Struct { .. }` or a `["…"]`, on a
  line with no target, on a step process with no end, and on one
  message repeating past a ceiling in one attempt at `info` or above.
- **Say what happened, not who is saying it.** The `target` column
  already names the module; `"opening the store"`, not
  `"doltlite_raw::open: opening {store}"`. A fan-in that runs once per
  producer checkpoint says one line per pass about what it found, and
  its per-source lines are `debug` unless that source moved.
- **A stamp you mint is UTC + offset**, never a bare local time
  (`IsoOffsetTimestamp::now_local().to_utc_and_offset()` in Rust,
  `nowIso()` in the UI).
