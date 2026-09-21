# The stores under a data root, and who owns each

```
<data_root>/<group>/ingest/entities.doltlite_db   per-source entities + sync bookkeeping
<data_root>/<group>/ingest/blobs.doltlite_db      content-addressed blobs
<data_root>/<group>/render_markdown/…             the rendered tree + its render store
<data_root>/unified_index/grid_index/db.doltlite_db   grid_rows / markdowns / edges / problems
<data_root>/unified_index/qmd_index/              the qmd index (plain SQLite inside)
<data_root>/system/feedback.doltlite_db           filed feedback
<data_root>/system/jobs.doltlite_db               the sync job queue
<data_root>/system/usage.doltlite_db              bytes-on-disk over time
<data_root>/system/runs.sqlite                    every run's step states, log lines and
                                                  metrics, plus the app server's own log;
                                                  every process — runner, step attempt,
                                                  server launch, page of the app — a
                                                  row in `processes`
                                                  (plain SQLite; any sqlite3 opens it)
<data_root>/system/dag_state.json                 the runner's record
<data_root>/system/api-token, lock, runner-lock   the server's token and the two flocks
```

One writer per file, and it is load-bearing: doltlite's working set is
per *file* and shared across processes, so two writers on one file
commit each other's in-flight rows. The `ingest` step owns its group's
two stores; `render_markdown` owns its render store; `grid_index` owns
the index; `datalib-http` owns feedback, jobs and usage; the applet only
reads, and reads at HEAD — one `dolt_hashof('HEAD')` per request, every
table through `dolt_at_<table>(hash)` — so a `grid_index` pass in flight
is never served. `runs.sqlite` is the exception because it is not doltlite: plain
SQLite in WAL mode, written by both the runner (its runs) and the server
(its own log), which SQLite's own locking makes ordinary.

The server's log includes one line per request it answered — method,
path, query, status and milliseconds, under the tracing target
`http.request` (`datalib/backend/http/src/request_log.rs`) — so
`target:http.request` in the log panel is the record of what the app
asked for. A read of the log itself is the one request that leaves no
line: the panel refetches whenever the log moves, and a line per
refetch would keep it moving.

A page of the app — one load in one browser tab — is a `ui` process
of its own, and what happened on it is its lines: `ui.page_load`,
`ui.navigate` (the path, which is the open card stack, so a search
typed into a grid is in it), `ui.error` for an exception nobody
caught, `ui.page_hide`. The page posts them in batches to
`POST /api/ui/events` (`datalib/ui/src/telemetry.ts` on one side,
`datalib/backend/http/src/ui_events.rs` on the other) and sends its
process id on every request as `X-Datalib-Page`, which the request
log keeps as `page` — the join from an action to what the server did
for it. The log panel lists pages beside the server's launches.

## The three stores `datalib-http` owns

`datalib-http` opens each through `sqlx::sqlite::SqlitePool` and wraps
them in `AppStore` (`datalib/backend/core/src/app_store.rs`), the
implementation of the `AppRepo` trait in `repo.rs`. The same pool serves
reads and writes.

**Feedback.** Every UUID-bearing UI surface has a "Feedback…" path; the
producer-side types and DOM breadcrumb walker are in
`datalib/ui/src/feedback/context.ts`, the row + discriminated payload is
`FeedbackRow` in `datalib/backend/app_schema/src/feedback.rs`. Each
`POST /api/feedback` inserts a row **and** runs
`SELECT dolt_commit('-Am', 'feedback: <uuid>')` on the same pooled
connection, so the commit covers exactly the row just written. What
makes that true is the **file**, not the connection: `-Am` commits
whatever else is dirty in the same file, which is why feedback has a
file of its own with one writer. Bazel stamps the binary with the git
hash via `tools/workspace_status.sh`; cargo builds get it from
`datalib/backend/core/build.rs`.

**Jobs.** The queue the UI fills and the worker drains; never committed.

**Usage** is the one store nothing ever commits. It is a timeseries —
`datalib-http` walks the root every five seconds *while a run holds it*
and appends a row per tree whose size moved — so the rows *are* the
history. Between runs nothing writes the root, so the series
deliberately has no samples there, and a change made from outside
datalib carries the instant it was next *measured*. Reading it is
`SELECT path, measured_at_utc, bytes FROM disk_usage`; it is compacted
(no repeated value, nothing closer than five seconds), so carry the last
value forward rather than assuming a fixed interval.

## Inspecting a store

**Stock `sqlite3` cannot open a `.doltlite_db`.** Use `datalib-doltlite`
(in the release tarball) or the Bazel-built shell, or turn any store
into plain SQLite with one pipe — `docs/dev/doltlite.md` has the
recipes and the warning about running anything against a store a sync
is writing.
