# Logging — one store, every process, and how to add a line

The run store: where every log line, step state and metric lands, who
writes it, how to add a line and how to read it. How a step writes
lines on stderr (pipes, envelopes, flushing, the error tail) is
[`step_protocol.md`](step_protocol.md#stderr-logging).

## One store

Everything any Datalib process says lands in one file,
`<data_root>/system/runs/runs.sqlite` — plain SQLite in WAL mode, so
`sqlite3` opens it. The runner writes it during a run; `datalib-http`
writes it for the life of the server. The tables:

| table | one row per |
|---|---|
| `processes` | a process that took part (below) |
| `runs` | a run of the runner: what it is for (`kind`: a sync, the prepare pass at a start, or giving back disk) and why it started (`reason`, also the run's first line). Empty for a run an older build recorded |
| `step_runs` | a step in a run: state, attempt, error, message. While a run is live every step of the config is here, `pending` until reached; when it ends, the ones no request reached go, so a closed run holds the steps it ran. A run whose runner died keeps them, as `stopped`: nothing settled them, so any might have been about to run |
| `log` | a line |
| `metrics`, `metric_samples` | a step's numbers — the newest value, and a sparse time series |
| `store_changes` | a part of the store a reader can depend on (`runs`, `step_runs`, `metrics`, `processes`, a run's lines, the server's lines), with a counter each write bumps |

Every stamp is UTC in a `*_utc` column with the offset the clock was
in beside it (`tz_offset`); text order is instant order. A line keeps
its own clock when it had one (a tracing envelope's `timestamp`, a
page's `at`), else the moment the writer saw it.

**Two writers share the file through SQLite's own locking.** A writer
that cannot take the lock waits ten seconds and is handed an error; a
batch that failed that way wrote nothing, so the writer offers it once
more, and if it gives up it logs how many lines went with it. Deciding
what to do with the file at open is exclusive between processes
(`runs.sqlite.open-lock`), and a store that has to be remade is emptied
in place under the file's lock, never deleted, so a reader with the old
file open cannot strand the new writer. `runs_two_process_test` runs
four writers at once and checks that every line reaches the store.

**A reader never waits on a writer**; that is what WAL buys. Every
Manage answer, chip and log page reads this file while a sync writes
it. On a busy disk, WAL cut a Manage answer's reads from a p95 of about
3.5 s to about 1 s. Recent rows can sit in `runs.sqlite-wal` until
SQLite folds them into the main file, so copy the store with
`sqlite3 … ".backup …"` or `VACUUM INTO`, never with `cp` alone.

SQLite's file locking is not dependable on a network or file-syncing
filesystem. **A data root belongs on local disk**, not on an NFS or SMB
mount or inside a Dropbox folder.

Retention is `[run_history]` in `config.toml`
([`dag_example.toml`](../../configs/dag_example.toml)): `max_runs` /
`max_age_days` for runs and everything that belongs to one,
`process_log_days` / `process_log_lines` for the lines outside any run
— the server's and the pages'. `disk_usage_days` is for the disk
samples in `system/disk_stats.sqlite`, not this store.

The store is not load-bearing. A store from an older schema version is
upgraded in place, keeping its rows. One that will not open, or is at a
version this build has no upgrade from (a newer build's, say), is
emptied and remade; the old file is first copied to
`runs.bak_<UTC stamp>.sqlite` beside it, which `sqlite3` reads and
nothing deletes, so remove those by hand. A writer that cannot open the
store at all is refused at its start with an ERROR saying why, and
nothing is recorded.

## Every line has an author

A log line names the **process** that wrote it (`log.process_id` →
`processes`), and the process row carries what a line should not
repeat: which program it was, when it started and ended, how it ended,
and the commit it was built from — what a line's `filename` and
`line_number` are relative to.

| `process` | what | who records it | ends when |
|---|---|---|---|
| `dag` | a run of the runner; or a `datalib-pipeline serve`, for the lines it writes outside any run | itself | it records nothing; the run row closes, or the serve stops |
| `step` | one attempt of one step — or one pass of a streaming consumer, which is spawned once per producer checkpoint; every pass is its own process, all under `attempt 1` | the runner, at spawn and at `wait(2)` | `exit_code` or `signal` |
| `http` | a launch of the server, and what its applets said | itself | it cannot see its own end |
| `applet` | one start of one applet, its config id in `applet`; it writes no lines, which stay the server's | the server, at spawn and when it stops or reaps it | `exit_code` or `signal` |
| `ui` | one load of the app in one browser tab | the server, on the page's behalf | the page says so on `pagehide` |

A process that was measured also carries **`peak_rss_bytes`**: the most
resident memory it and everything below it held. The runner fills it
for a step's attempt from the step's `rss_peak_bytes` series, and the
server for an applet from its own sampling every five seconds. Each
writes the row again as the peak rises, so a process whose watcher died
still leaves its peak so far. To ask which process held the most:

```sh
sqlite3 <root>/system/runs/runs.sqlite \
  "SELECT process, COALESCE(step, applet), attempt, peak_rss_bytes / 1048576 AS mib
     FROM processes WHERE peak_rss_bytes IS NOT NULL
    ORDER BY peak_rss_bytes DESC LIMIT 20"
```

An applet's row has no lines, so the retention that prunes the server's
lines prunes it too, `process_log_days` after it started.

A line also keeps its **subject** — `run_id`, `step`, `attempt` — which
is not the same thing: the runner's line "step X failed" is authored by
the runner and about step X. A line about a step also carries the
step's `group_id`, so a source's lines can be read together whatever
step wrote them.

The commit belongs to the process, not the store, because lines from
different builds sit in one file. It comes from the
`DATALIB_GIT_HASH` environment variable, else a `git-hash` file beside
the binaries, which a release carries; the process logs which at boot
(`build commit read from …`). One with neither warns and records
nothing, and the log's source links point at `main`. A step attempt's
commit is the `version` its program reports in a `capabilities` event
(`step_protocol.md` §"stdout: the event protocol"): `datalib-step`
reports the commit it was built from, and a program that reports none
has none. A page has the server's.

## Where a line comes from

| you are writing | do this | it becomes |
|---|---|---|
| Rust in the runner or the server | `tracing::info!(target: "…", key = value, "the sentence")` | a row with `target`, `msg`, and the keys as a JSON object in `fields`; `filename` / `line_number` added |
| the runner, about a step — a checkpoint sealed, a pass that failed, why it ended, a hint | nothing — the runner writes these itself | `target:datalib_dag::runner` under the step, with the runner as author |
| Rust in `datalib-step`'s steps | the same `tracing` call | a JSON envelope on the step's stderr, which the runner unwraps into the same columns; the line's own timestamp wins |
| Rust in an applet (`datalib-applet`) | the same `tracing` call | a JSON envelope on the applet's stderr, which the gateway logs again as the server's line at the envelope's level: `target:datalib_http::applets`, with `applet` naming it and the applet's own target and fields in `applet_target` and `applet_fields`. The row's `filename` / `line_number` are the gateway's; the applet's are inside `applet_fields` ([`applets.md`](applets.md)) |
| a custom step, any language | print a line on stderr (or a non-event line on stdout) | an `info` row with `stream` set; the last lines before a non-zero exit also become the step's error |
| the server, per request | nothing — the server logs every request | `target:http.request`: method, path, query, status, `ms`, `bytes`, the `page` that asked, and the `card` and `card_type` when a card asked (`ui.card_open` says what source that card ran). These are pages asking this server, not traffic off the machine. `debug` when it succeeded and only read (below), `info` for what changes something or was refused, `warn` for a 5xx |
| Rust that reaches a web service | nothing: every request goes through `datalib_etl_web::http`, which says so | `target:http.outbound`, whose lines start `outbound:`. One `debug` line per request, with its `url` (no query), `status` and `duration_ms`; and at `info`, per host, a line at the first request, one a minute while it goes on, and the whole at the step's end. A remote image the server fetches is an `info` line naming its host alone. A request played back from a test's tape is not outbound |
| the UI | `track("name", { …fields }, { level, msg })` from `ui/src/telemetry.ts` | `target:ui.name` under the page's own process, with the page's clock; batched, `keepalive`, never throws |

Adding a UI event is one word in the `PageEventName` union and the
call; the server files any word. Uncaught exceptions, route changes
and every toast shown (`target:ui.toast`, its text as the line, at its
level) are already tracked.

So are the two waits a person sits through most. `ui.search_answered`
is a search card's rows reaching the screen; `ui.doc_shown` is a
document's body drawn in its frame. Each line has `ms`, `outcome`
(rows, refused or failed for a search; shown or failed for a document)
and `trigger`: `open` counts from the card opening, `search` from the
card asking a search it has not shown. A wait of two seconds or more is
a `warn`, so `target:ui.search_answered min_level:warn` in the log card
lists the slow ones. On a page in the background, which draws nothing,
the wait ends when the answer arrives and the line says `hidden`.

**Levels** are `trace` … `error`. The level a root logs at is
`log_level` at the top of its `config.toml` — `trace` when it does not
say — and it applies to Datalib's own crates (the server's `http.*`
targets included) in every process of that root: the server reads it at
launch (a change takes a restart), the runner per run, and each step
gets the resulting filter as `RUST_LOG`. Third-party crates follow it
down to `info` and no further, and the noisy ones (`sqlx`, `hyper`,
`html5ever`, …) stay at `warn` or above. A `RUST_LOG` set where a
process was started wins over all of that.

The store keeps every level it is handed — `debug` is where a doltlite
commit or a batch of rows goes — and the log card opens at
`min_level:info`, so the lower levels are there when asked for. Little
emits `trace`: a step's progress ticks, so a log with none is normal.
The server's `debug` lines in ordinary service are a page reading what
it draws — any `GET`, the few `POST`s that only ask (chiefly
`/api/entities`), a layout saved under `/api/ui/state/`, a refetch on a
`root` frame — and the disk walk (`usage: walked the data root`), every
five seconds while a sync runs. So the card's default view of a request
is one that changed something, went wrong or left the machine. A
`GET /api/manage/rows` that takes a second or more also writes an
`info` line, "the Manage rows were slow to assemble", splitting its
time between its parts.

**A slow statement is `debug`, and the attempt's end sums them.** sqlx
warns of every statement over a second, and a render over a big store
is one such read per page. The runner keeps each at `debug`, out of the
step's count of warnings, and writes one `info` line as the attempt
ends: how many, how long in all, and the statement most of the time
went to, in `top_statement`. Under five seconds in all, that line is
`debug` too. The server and its applets, which never end, keep sqlx's
own `warn`.

**A heavy statement is summed the same way.** Every binary counts what
each SQLite statement did (`datalib_sql_work`): the steps SQLite's engine
ran, about three for each row it walks, and the rows the statement
answered with. One that ran 100,000 steps or more, and 100 or more for
each row it answered, writes a `warn` line under the target
`datalib::sql_work`: the `statement`, the `file` it ran on, its
`params` by shape (`text(12)`, `json[9577]`, `int`) and SQLite's
counters, `fullscan_steps` among them. Work, unlike time, does not
change with a cold disk, a lock or a busy machine, so such a line is a
plan to look at: a missing index, or one that does not narrow the
search. The runner keeps each at `debug` and writes one line as the
attempt ends, naming the statement that did most of the work and its
file; under ten million steps in all, that line is `debug` too. A bound
value can be anything, a secret included, so the line names its shape;
`DATALIB_SQL_VALUES=1` writes the values in place, for a session on
one's own library.

**A migration says so from its start.** A step asked to migrate first
says `migrate: updating the raw store for this build; nothing is
downloaded`, as a line and as its status. Each rung then says, before
it runs, where the climb starts and ends and how big the store is —
`migrating the store (13.0 GB) from version 7 to 9, now version 8:
<its name>` — and `migrated`, with its `ms`, after. A rung written as
several statements says `migrating: statement 3 of 4: <what it does>`
before each. Every one of these is the step's status too, so the
Manage row says which rung and statement is running.

**A render says where its time went.** Every render ends with one line
of its `total_ms` and `write_ms`. A Claude Code or Codex render also
says `where the render's time went`, in `load_ms` (the store's answer),
`parse_ms` (that answer into values), `render_ms` (all the rest) and
`write_ms`, with a `debug` line for each page.

## What is not a log line

- **Anything about a record** — a fetch that failed, a render that
  could not project a field — is a **problem** row, kept in the store
  of the step that found it
  ([`step_protocol.md`](step_protocol.md#problems-what-you-could-not-do)).
  It reaches the Manage counts and the document's banner; a `warn!`
  reaches nobody who is not reading the log, so do not write one beside
  a problem. The step logs what is new, once, at its end: one
  `problems_recorded` line per kind of problem (stage, reason, rule,
  field) with its `count`, at its loudest row's severity (`error`,
  `warn`, or `debug` for a finding), and never a row's `sample` or key,
  which hold the record's own contents. A row that says what it said
  last run is not news and says nothing in the log. A writer that
  stores a row it made, new or changed (`ProblemRow::is_news`), calls
  `datalib_problems::note_recorded`.
- **A number** — rows written, requests made, queue depth — is a
  `metric` event, not a sentence with a number in it. The Manage
  screen's queue and ETA and the sync dashboard's charts come from
  `metric_samples`.

## What a line may carry

The log is the person's own and stays on their machine, under their
data root, beside the mirror it describes. So a line may carry whatever
helps someone work out what happened: a search they typed, a file path,
a record's subject, an account's name.

- **Never a secret**: a token, a password, a cookie, an API key. Do it
  by construction, not by scrubbing a pattern out afterwards: print a
  URL as a `ShownUrl` (scheme, host, path), since a signed URL carries
  its credential in the query. The request log drops `?token=`.
- **The home directory is written `~`** in every line's message and
  fields and in a step's error, where lines enter the store, so you
  need not do it yourself. It only matches a whole path prefix:
  `/Users/alice` stays when the home is `/Users/al`.

Because a log can hold a person's data, it is theirs to share: before a
log goes into a public issue, the person reads it first.

## Reading it

**In the app.** The log is a card, `logView({ run, step, launch })`.
**Logs** in the status bar opens it on everything (the picker holds the
runs, this server's launch and earlier ones, and the pages of the app);
on the Manage screen, **Show step log** on a step's menu opens its
newest attempt, as does a double-click on a Failed row. A double-click,
Enter or "Open this line" opens `logLineView(seq)` beside the card: the
whole message, the fields as a tree, the source link at the process's
commit, both clocks. The grid shows Time, Step, Level, Stream, Source,
Message and Fields; the grid menu at the top right shows the hidden
ones (run, process, commit, thread, target, group).

The search bar takes the grammar every grid shares
([`searching.md`](../user/searching.md)): the keys are the columns —
`run`, `process`, `step`, `group`, `level`, `stream`, `target`,
`thread`, `msg` — plus `process_id` and `attempt`, `min_level:warn`
(this level and above) and `commit:0fc29cb` (prefix). The pickers above
the grid write `run:`, `process_id:`, `step:` and `attempt:` into the
query, and clearing it shows the whole store. The card tails while what
it shows may still be writing.

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

On `/api/log`, `q` is the whole of what is asked, in the search bar's
grammar; any parameter but `q`, `limit`, `after_seq` and `before_seq`
is refused. With no cursor the answer is the newest `limit` lines;
`before_seq` pages back from the oldest one held, and `after_seq` is
the tail cursor: remember the last `seq`, ask again on the live
stream's `table_changed: log` frame.

**From a shell**, since it is plain SQLite:

```sh
sqlite3 <root>/system/runs/runs.sqlite \
  "SELECT l.ts_utc, p.process, l.level, l.target, l.msg
     FROM log l LEFT JOIN processes p USING (process_id)
    ORDER BY l.seq DESC LIMIT 50"
```

### From outside the app

`GET /metrics` serves the numbers in Prometheus's text exposition
format, which Prometheus, Grafana's agent and an OpenTelemetry
collector's Prometheus receiver all read. It is behind the API token
like every route, so a scrape sends it as a bearer token:

```yaml
scrape_configs:
  - job_name: datalib
    static_configs: [{ targets: ["127.0.0.1:8731"] }]
    authorization: { credentials_file: <root>/system/api-token }
```

What it serves:

- **`datalib_step_<name>`**: every series a step the config declares
  has reported, its newest value from the last run it reported in,
  labelled `step`, `group` and the series' own labels. One whose name
  ends in `_total` is a counter, anything else a gauge
  ([`step_protocol.md`](step_protocol.md#stdout-the-event-protocol-optional)
  has the naming rules). A step's counters start again from zero each
  run, which a scraper reads as a counter reset.
- **`datalib_step_state{state=…}`**: 1 for the state the sync loop last
  put the step in, 0 for the others — `running`, `failed`, `off`, …
- **`datalib_step_last_success_timestamp_seconds`**: for an alert on a
  source that has not synced in a day.
- **`datalib_tree_bytes{tree=…}`** and **`datalib_root_bytes`**: what
  the usage sampler last measured.
- **`datalib_disk_free_bytes`**, **`datalib_disk_size_bytes`**,
  **`datalib_disk_pause_below_bytes`**, **`datalib_disk_resume_at_bytes`**
  and **`datalib_disk_low`**: the data root's disk and the config's
  `[disk_space]` floor
  ([`dag/README.md`](../../datalib/backend/dag/README.md#when-the-disk-runs-low));
  `datalib_disk_low` is 1 while the steps are held.
- **`datalib_applet_rss_bytes{applet=…}`** and
  **`datalib_applet_rss_peak_bytes{applet=…}`**: the resident memory of
  each running applet and every process below it, read every five
  seconds, and the most since it started. A step's own memory is its
  `rss_bytes` series.

Its history starts when something begins scraping; the app's own views
read the run store, which has every run it keeps. Labels are step and
group ids and what a step labels its series with (a table name, a
producer), never a record's contents. Log lines do not go out this way.

## Rules

For anyone adding a line, an event or an endpoint:

- **Fields, not interpolation.** `job = %id, "claim failed"` is a
  `fields.job` anyone can filter and group on; `"claim failed for
  {id}"` is a sentence. And a sentence, always: a line whose only
  message is an `event = "name"` is found by neither a `msg:` search
  for words nor one for the name. Keep `event` as a field beside the
  sentence where a name helps.
- **A field is a value, not a `Debug` rendering.** `?opt` puts
  `Some(Origin)` in the store, which nothing can filter on; write
  `opt.map(Reach::as_str)`, `.as_deref()`, or `%list.join(",")`. A
  fixture test fails on a `Some(`, a `None`, a `Struct { .. }` or a
  `["…"]`, on a line with no target, on a step process with no end, and
  on one message repeating past a ceiling in one attempt at `info` or
  above.
- **One request or one record is `debug`.** A line per page, thread or
  batch is hundreds at `info` on a real sync. Say what it came to, once,
  at the end.
- **Say a standing fact once.** What is still true on the next pass is a
  `problems` row, which a person sees as long as it holds; the log says
  when it became true.
- **Say what happened, not who is saying it.** The `target` column
  already names the module: `"opening the store"`, not
  `"doltlite_raw::open: opening {store}"`. A fan-in that runs once per
  producer checkpoint says one line per pass about what it found, and
  its per-source lines are `debug` unless that source moved.
- **A stamp you mint is UTC + offset**, never a bare local time
  (`IsoOffsetTimestamp::now_local().to_utc_and_offset()` in Rust,
  `nowIso()` in the UI).
- **A step flushes per line**
  ([`step_protocol.md`](step_protocol.md#stderr-logging)).
- **A response that is the log must not write the log.** The card
  refetches on every `log` frame; a request line for `/api/log` would
  move the store, which is the frame, which is the next request. The
  request log skips `/api/log` and `/api/runs/{run}/log`; a new
  endpoint that serves log rows joins that list
  (`http/src/request_log.rs`).
- **A loop that gets past that rule is counted.** A fetch the page
  makes while handling a live frame sends `X-Datalib-Cause`, the
  frame's chain length, and the server stores a nonzero one as
  `fields.chain`. At 5, and again at 50, 500 and so on, it writes a
  `warn` with target `http.loop` naming the endpoint and the page:
  search `target:http.loop` to find one.
- **A card off screen is not told.** A card that subscribes with
  `subscribeLive(handlers, { onScreen: el })` has its frames held while
  `el` is off screen and delivered, once each, when it is back. Every
  card that refetches on a frame passes its root element.
- **A tab the server refuses goes quiet.** A tab left open across a
  restart holds the old token, so each request it makes is a 401 and a
  request line. The live stream backs off (1 s, doubling to 30 s), and
  the first 401 from `/api/` ends the tab's session with one toast.
