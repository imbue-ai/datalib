# The stores under a data root, and who owns each

```
<data_root>/<group>/ingest/entities.doltlite_db   per-source entities + sync bookkeeping
<data_root>/<group>/ingest/blobs.sqlite           content-addressed blobs (plain SQLite)
<data_root>/<group>/render_markdown/…             the rendered tree + its render store
<data_root>/unified_index/grid_index/db.doltlite_db   grid_rows / markdowns / edges / problems /
                                                  source_contacts (who each handle is, per source)
<data_root>/unified_index/grid_index/search_terms.sqlite every id, handle, title, label and name each
                                                  grid row answers to, full-text indexed, and the
                                                  names each handle went by (plain SQLite;
                                                  `grid_index` rewrites it after each pass,
                                                  `etl/render/src/search_terms.rs`)
<data_root>/unified_index/qmd_aggregator/           the qmd index (plain SQLite inside)
<data_root>/unified_index/embedding_map/embedding_map.json
                                                  every embedded document's place on
                                                  the map card; replaced whole by
                                                  each run, deleted by a reset
<data_root>/datalib_curated/datalib_contacts/contacts.doltlite_db
                                                  contacts a person made, the handles
                                                  linked to them and the photo they put
                                                  on one; written only by the
                                                  `datalib_contacts` applet
<data_root>/system/feedback.doltlite_db           filed feedback
<data_root>/system/disk_stats.sqlite              bytes on disk, and free space on
                                                  the disk, over time (plain SQLite;
                                                  any sqlite3 opens it)
<data_root>/system/remote_media.doltlite_db       what remote media a person let a document
                                                  load, and the URLs fetched for it
<data_root>/system/remote_media/<sha256>          the download CAS those URLs' bytes land in
<data_root>/system/runs/runs.sqlite               every run's step states, log lines and
                                                  metrics, plus the app server's own log;
                                                  every process — runner, step attempt,
                                                  server launch, page of the app — a
                                                  row in `processes`
                                                  (plain SQLite; any sqlite3 opens it).
                                                  Its own directory, so the journal beside
                                                  it counts with it on the Manage screen,
                                                  as does each `runs.bak_<stamp>.sqlite`: the
                                                  old file, copied aside when a new build
                                                  or a torn file made it start over
<data_root>/system/supervisor.sqlite              requests (every sync anyone asked for,
                                                  and how it ended) and steps turned off: the
                                                  mailbox the loop reads; and the loop's
                                                  record — each step's state now, its
                                                  last run and last success, each
                                                  sink's version,
                                                  the run in flight, every process it
                                                  started (plain SQLite; anyone writes
                                                  intent, only the loop's holder writes
                                                  the record; every commit is announced)
<data_root>/system/supervisor-listeners/          a FIFO per process waiting on the loop's
                                                  store: how it hears a commit
                                                  (`dag/README.md` § What wakes the loop)
<data_root>/system/api-token, lock, runner-lock   the server's token and the two flocks
                                                  (the server holds both while it is up)
<data_root>/system/ui-state/<name>.json            JSON the UI keeps in the library: the
                                                  containers layout's open tree
                                                  (`layout`) and saved composites
                                                  (`composites`); opaque to the server
                                                  (`http/src/ui_state.rs`)
<data_root>/system/library-summary.json           source count, bytes on disk and the last
                                                  sync's end, for the desktop app's list of
                                                  libraries; datalib-http rewrites it when
                                                  it answers the manage rows and a figure
                                                  moved (`http/src/manage/summary.rs`)
```

Every store above but qmd's index and the JSON files carries a
`_datalib_meta` table — which datalib and
git commit wrote it, the doltlite it was written with, a hash of the
DDL it was opened with, and its kind — written by the owner on open
and committed with the schema (`datalib_store_meta`;
[`plans/completed/schema_migrations.md`](plans/completed/schema_migrations.md) §3.1). A reader that wants to
know what it is looking at reads that before its first query — and
every owner does, refusing a store a newer `major.minor` of datalib
wrote (`datalib_store_meta::guard`; the app server then boots only to
show the screen that says so, and `datalib-dag` refuses the root).

`datalib_curated/` holds what a person curates by hand, one directory
per app, so each can be managed or deleted on its own. Nothing can
rebuild it: no reset or source deletion touches it, and its store
refuses a schema it cannot reach rather than rebuilding
(`doltlite_raw::open_curated`). No group or step may claim the name
(`datalib_dag::config::CURATED_DIR`).

One writer per file ([`etl/README.md`](../../datalib/backend/etl/README.md)
§ "Connection pools" has the rule and why). The `ingest` step owns its group's
two stores; `render_markdown` owns its render store; `grid_index` owns
the index; `datalib-http` owns feedback, usage and remote media;
the applet only reads, each request inside one read transaction on a
read-only connection, which holds one commit (`DoltRepo::pinned` in
`unified_index/src/dolt_repo.rs`;
[`doltlite.md`](doltlite.md#three-ways-to-read-one-commit)) — so a
`grid_index` pass in flight is never served. `runs.sqlite` is the exception because it is not doltlite: plain
SQLite in rollback-journal mode, written by whoever runs the loop (its
runs) and the server (its own log) — one process while the server is
up, two while a `datalib-dag` runs the loop — which SQLite's own locking
makes ordinary; `runs_two_process_test` is the measurement.

Who writes which line of it, how to add one, and how to read it is
[`logging.md`](logging.md).

## The three stores `datalib-http` owns

Feedback and remote media are doltlite; disk stats is plain SQLite.
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
whatever else is dirty on the branch in that file, from any process
([`doltlite.md`](doltlite.md#branches-head-and-the-working-set)), which
is why feedback has a file of its own with one writer. The row's `git_hash` is the build's
commit as `datalib_runtime::build_id::git_hash` finds it at run time
([`logging.md`](logging.md) § "Every line has an author").

**Disk stats** is plain SQLite, because nothing ever committed it: it
is a timeseries — `datalib-http` walks the root every five seconds
*while a run holds it* and appends a row per tree whose size moved — so
the rows *are* the history. Between runs nothing writes the root, so the series
deliberately has no samples there, and a change made from outside
datalib carries the instant it was next *measured*. Reading it is
`SELECT path, measured_at_utc, bytes FROM disk_usage`; it is compacted
(no repeated value, nothing closer than five seconds), so carry the last
value forward rather than assuming a fixed interval. Beside it,
`disk_free` holds the free space on the root's disk, looked at every ten
seconds whether or not a run is going and recorded when it moves by
10 MB or more (`SELECT measured_at_utc, available_bytes, total_bytes
FROM disk_free`). A root from before this store kept both tables in the
doltlite `system/usage.doltlite_db`; the first open by a newer build
copies its rows across and removes it, or, if it cannot read it, logs
that at ERROR and tries again next time
(`core/src/app_store_migrate.rs`).

**Remote media.** A rendered document's images on remote
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
