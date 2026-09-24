# The stores under a data root, and who owns each

```
<data_root>/<group>/ingest/entities.doltlite_db   per-source entities + sync bookkeeping
<data_root>/<group>/ingest/blobs.doltlite_db      content-addressed blobs
<data_root>/<group>/render_markdown/…             the rendered tree + its render store
<data_root>/unified_index/grid_index/db.doltlite_db   grid_rows / markdowns / edges / problems
<data_root>/unified_index/qmd_index/              the qmd index (plain SQLite inside)
<data_root>/system/feedback.doltlite_db           filed feedback
<data_root>/system/usage.doltlite_db              bytes-on-disk over time
<data_root>/system/remote_media.doltlite_db       what remote media a person let a document
                                                  load, and the URLs fetched for it
<data_root>/system/remote_media/<sha256>          the download CAS those URLs' bytes land in
<data_root>/system/runs/runs.sqlite               every run's step states, log lines and
                                                  metrics, plus the app server's own log;
                                                  every process — runner, step attempt,
                                                  server launch, page of the app — a
                                                  row in `processes`
                                                  (plain SQLite; any sqlite3 opens it).
                                                  Its own directory, so the WAL beside
                                                  it counts with it on the Manage screen
<data_root>/system/supervisor.sqlite              requests (every sync anyone asked for,
                                                  and how it ended) and pauses: the
                                                  mailbox the loop reads; and the loop's
                                                  record — each step's state now, its
                                                  last run and last success, each
                                                  sink's version,
                                                  the run in flight, every process it
                                                  started (plain SQLite; anyone writes
                                                  intent, only the loop's holder writes
                                                  the record)
<data_root>/system/supervisor-bells/             one FIFO per process listening for
                                                  writes to supervisor.sqlite; every
                                                  writer rings them all (`bell.rs`)
<data_root>/system/api-token, lock, runner-lock   the server's token and the two flocks
                                                  (the server holds both while it is up)
```

Every store above carries a `_datalib_meta` table — which datalib and
git commit wrote it, the doltlite it was written with, a hash of the
DDL it was opened with, and its kind — written by the owner on open
and committed with the schema (`datalib_store_meta`;
`docs/dev/plans/completed/schema_migrations.md` §3.1). A reader that wants to
know what it is looking at reads that before its first query — and
every owner does, refusing a store a newer `major.minor` of datalib
wrote (`datalib_store_meta::guard`; the app server then boots only to
show the screen that says so, and `datalib-dag` refuses the root).

One writer per file, and it is load-bearing: doltlite's working set is
per *file and branch* and shared across processes, so two writers that
land on one branch commit each other's in-flight rows. The `ingest` step owns its group's
two stores; `render_markdown` owns its render store; `grid_index` owns
the index; `datalib-http` owns feedback, usage and remote media;
the applet only reads, and reads at HEAD — one `dolt_hashof('HEAD')` per request, every
table through `dolt_at_<table>(hash)` — so a `grid_index` pass in flight
is never served. `runs.sqlite` is the exception because it is not doltlite: plain
SQLite in WAL mode, written by whoever runs the loop (its runs) and the
server (its own log) — one process while the server is up, two while a
`datalib-dag` runs the loop — which SQLite's own locking makes ordinary —
`runs_two_process_test` is the measurement, not the argument.

Who writes which line of it, how to add one, and how to read it is
[`logging.md`](logging.md).

## The four stores `datalib-http` owns

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

**Usage** is the one store nothing ever commits. It is a timeseries —
`datalib-http` walks the root every five seconds *while a run holds it*
and appends a row per tree whose size moved — so the rows *are* the
history. Between runs nothing writes the root, so the series
deliberately has no samples there, and a change made from outside
datalib carries the instant it was next *measured*. Reading it is
`SELECT path, measured_at_utc, bytes FROM disk_usage`; it is compacted
(no repeated value, nothing closer than five seconds), so carry the last
value forward rather than assuming a fixed interval.

**Remote media** (issue #648). A rendered document's images on remote
hosts are held back by the UI until a person lets them load, because
loading one tells its host who opened the document and when. A
decision is a row in `remote_media_allow` — its `scope` is `url`,
`document`, `host` or `source` and its `key` the thing named. The
server is the one judge of what a row covers
(`http/src/remote_media.rs`): the document view asks it which of a
document's references may load (`POST /api/remote_media/check`, with
the document's `markdown_uuid` and source id, since a `document` or
`source` row covers only what is loaded for it) and renders under the
answer; and `GET /api/remote_media?url=…&document=…&source=…` refuses
a URL no row covers before it fetches or serves anything. A covered
URL is fetched once (no cookie or referrer, media types only, a size
cap, redirects re-judged per hop, private and loopback targets
refused), its bytes kept at `system/remote_media/<sha256>` and a
`remote_media` row saying so; every later request is answered from
there, so the host hears of it once. Both tables are committed per
write like feedback, are served as typed tables at
`/api/remote_media/allow` and `/api/remote_media/fetched`, and an allow
row is deleted through `DELETE /api/remote_media/allow/{uuid}` — the
document banner offers that for the rows in effect.

## Inspecting a store

**Stock `sqlite3` cannot open a `.doltlite_db`.** Use `datalib-doltlite`
(in the release tarball) or the Bazel-built shell, or turn any store
into plain SQLite with one pipe — `docs/dev/doltlite.md` has the
recipes and the warning about running anything against a store a sync
is writing.
