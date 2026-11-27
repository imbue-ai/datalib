# The Sources table is held in memory and rebuilt only when its inputs move

*Proposal (2026-11-27). Order step 0 is built (`pass_changed` on every
record move). Step 1 is built (`group::busy_label`, `InSync`), with
step 4's Ingest label: the server names the step "Download" or
"Import" (`manage/ingest_word.rs`) and the browser's own swap is gone.
The rest is not built. The numbers come from one
real library that day: 21 sources, 88 steps, a 400 MB run store. They
were read from the server's own "the Manage rows were slow to assemble"
lines and its request log (`docs/dev/logging.md`). Facts about the tree
were read the same day; check one before relying on it.*

## The problem

The Sources page is often slow to open, and it gets slower during a
sync. Two things cause this.

**Every answer is built from scratch.** `GET /api/manage/rows`
(`http/src/manage/mod.rs`, `manage_rows`) reads every input again on
each call:

- the loop's record (`system/supervisor.sqlite`);
- the run store (`system/runs/runs.sqlite`), opened about ten times;
- every step's problems, by walking the step folders;
- the config, parsed twice;
- the requests its steps name, one lookup each.

Then it joins them. On the library above, each part took this long:

| part | time |
|---|---|
| the record | 1–6 ms |
| requests | 0–19 ms |
| joining the rows | 2–6 ms |
| the run store's metrics (Problems, Items, Modal spend, owed) | up to 1.2 s |
| the run store's progress for the run in flight | up to 1.9 s |

A slow answer took 1.1–2.5 s, and nearly all of it went to the run
store. Its slowest statements were `runs.list_metrics_of_run`,
`runs.newest_metric_per_series_by_name` (five times per answer),
`runs.count_problem_lines_per_step` and `runs.metric_history_since`.
Opening the page pays all of this, even when nothing has changed since
the last time it opened.

**Each change is paid for several times.** Every write to the loop's
record, and most writes to the run store, send a `manage.rows` frame,
at most one a second. Each reader then fetches on its own:

- the Sources card (`sourcesCardModel.ts`);
- a source's own card (`SourceCard.ce.vue`);
- the dashboard (`useDashboard.ts`);
- the Library card, a chip's menu (`chipActions.ts`) and the Perseus
  view, once each when opened;
- the sync pill (`SyncProgressChrome.vue`), on every `dag` frame,
  which is not throttled;
- chips, through `POST /api/entities`. It rebuilds the whole table to
  answer a few names (`manage/mod.rs`, `post_entities`).

During one grid-index rebuild the server answered about 55
`/api/manage/rows` and 55 `/api/entities` a minute. Each one read about
18,800 rows and sent about 264 KB.

Building everything per request also brings three smaller problems:

- **A failed read comes back as nothing.** If the record cannot be
  read, every row says "Never run". If the mailbox cannot be read,
  every request is forgotten, so Sync is offered while a sync runs. A
  failed problem count draws no counts. A config that cannot be read
  draws an empty table. All four break the rule "No fallbacks, no
  workarounds" in `AGENTS.md`.
- **Facts from the record travel on other endpoints, so they go
  stale.** `/api/config` carries the steps left to re-run
  (`upgrade.rerender`), read from the record. Only config and pass
  events made the page fetch it again, so the "1 source to re-run"
  pill stayed after its step had run. `library-summary.json` is written
  as a side effect of the GET, so it only moves while a Sources view is
  open.
- **The browser re-derives what the server sends.** It parses the TOML
  itself to count sources (`emptyTableDiagnosis` exists only because
  that count can disagree with the server's). It also renames "Ingest"
  to "Download" or "Import" on the Sources card alone, so a chip for
  the same step still says "Ingest".

## The design

**The server keeps the table in memory and rebuilds it when an input
moves; nothing else rebuilds it.** The watcher (`http/src/watch.rs`)
already knows when each input moves:

- the record's announcements;
- the run store's write counter for each part (`datalib_runs::versions`,
  `StorePart`);
- the config file;
- a curated commit (the problem mutes);
- the usage sampler.

Today it turns those into frames, and each reader rebuilds the table
for itself. Instead, the watcher hands each move to one owner of the
table, `SourcesTable`, kept in `AppState`.

### What the owner holds, and what moves each part

| input | read again when | how much is read |
|---|---|---|
| the config, as written and checked | `Moved::Config` | the file, parsed once |
| the record and its requests | `Moved::Supervisor` | the record; only the requests it names that are not held yet |
| progress of the run in flight | `StorePart::StepRuns` or `RunLog` | the current run only (`snapshot_of(run)`) |
| metrics: Items, Modal spend, owed, problem lines | `StorePart::Metrics` | the current run's rows only, through the key's `run_id` prefix (`sqlite_autoindex_metrics_1`); merged into the newest-per-series map held |
| metrics of earlier runs | once at start, and when `StorePart::Runs` shows a run gone | every run, as today |
| problems in the step stores | a step's run ends (its `last_run` moves in the record), or `Moved::Curated` | that step's stores only (`step_problems` already caches by mtime) |
| storage, applet errors, pass state | already in memory | — |

Every input that moved is read once. Then the rows are joined again
(`Snapshot::rows`, already pure, 2–6 ms), serialized once, and given a
new version. One `manage.rows` frame goes out. `GET /api/manage/rows`
returns the held bytes and runs no SQL, so opening the page costs a
memory copy.

**Rows that end up the same send no frame.** The owner compares the new
bytes with the old before bumping the version. A log line that changes
nothing on the table then wakes no reader. Today every `RunLog` write
wakes the Manage rows.

**A failed read keeps the last good value, and says so.** Each input
holds its last good value and its last error. The response gains
`stale: [{input, error, since}]`, and the card draws a banner naming
it. Nothing reads as "Never run" because a file was locked for a
moment. This replaces the four silent defaults above.

### One reader in the browser

`ui/src/sourcesRows.ts` holds the one copy of the table: one
subscription, one fetch per frame, and the version it last drew. Every
reader in the list above takes its rows from there. Chips resolve a
`datalib:group/…` or `datalib:step/…` link against the same rows, so
`POST /api/entities` and `entities.ts`'s fetch and cache go.

### The facts the record holds go on the rows

- **"Needs re-run" is a row status**, read from `StepRecord.needs_rerun`
  in the record, like every other status. The upgrade screen and the
  pill read it from `sourcesRows.ts`. `/api/config`'s `upgrade.rerender`
  and `http/src/lib.rs`'s `rerun_offer` go, and so does the widened
  `pass_changed` from the stopgap below.
- **`library-summary.json` is written when the table is rebuilt**, not
  when someone fetches it.
- **The label is the server's.** The Ingest step's name ("Download",
  "Import") is decided where the row is built, so the table, chips and
  the dashboard all say the same thing. The browser's TOML parse for
  the source count goes too; the response already says what the config
  holds.

### "3/4 complete" on a group while it runs

While a group reads Running, Waiting or Queued, its Status says how far
this sync has got through the group's steps, e.g. "Running · 2/3
complete". This is a pure addition to `group::group_status`, worked out
from what the record already holds:

- a step **counts** if it is on and either ran in the current run
  (`CurrentRun.states`) or names an open request (`StepRecord.requests`);
- it is **complete** once its state in the current run is terminal and
  it names no open request. `CurrentRun.states` holds the state as text,
  so it is read with `RunState::parse` and checked with `is_terminal`.

The count uses the record alone, so it needs no graph walk. A step in
another group that the sync reaches, such as the grid index, counts
under its own group. Steps turned off are left out, as the group's
status already leaves them out.

## What goes

- `POST /api/entities`, `post_entities`, `entity_view`, and the fetch,
  cache and revalidation in `entities.ts`.
- `fetchManageRows` in five readers, each with its own guard against
  older answers.
- `/api/config`'s `upgrade.rerender`, `http/src/lib.rs`'s
  `rerun_offer`, and `pass.ts`'s `rerenderSources`. The UI's
  re-run view reads the rows.
- The run-store reads in `dag_record` that are made on every request.
  `/api/dag` reads the held record too, and stops reading metrics it
  throws away.
- The four silent defaults (`unwrap_or_default` on the record,
  `HashMap::new()` on a mailbox error, `None` problem counts, `""` for
  an unread config).
- `summary::record` on the GET path.
- The browser's TOML parse for the source count, `emptyTableDiagnosis`,
  and the Sources-card-only `ingestLabel` swap.
- `RunLog → ManageRows` in `watch.rs::tables_of`. The owner decides
  whether a log line moved the table.

## What stays

- **`Snapshot::rows`** and the pure status functions (`step_status`,
  `group_status`, the queue cells). The owner calls them exactly as
  `manage_rows` does now.
- **The column spec**, which declares the columns the UI draws.
- **The watcher's classify → expand → frames pipeline and its
  throttle.** The owner sits between expand and frames.
- **`POST /api/manage/rows/refresh`** keeps its one job, measuring the
  disk now. It then answers from the table like the GET.

## Order of work

0. **A stopgap that can ship now.** Send `pass_changed` on every record
   move, so the re-run pill clears without a reload. This is the
   uncommitted change in `watch.rs`, which also deletes
   `is_wipe_line`. Step 4 removes its reason to exist.
1. **"N/M complete" on running groups**, in `group::group_status`, with
   tests next to its existing ones. It stands alone.
2. **The owner on the server.** `SourcesTable` holds the inputs and the
   serialized rows; the GET, `/api/dag` and the refresh POST answer
   from it; the silent defaults become `stale`. Test: a GET after
   nothing moved runs no statement. The `sql_work` counters can show
   that. Test: each `Moved` reads only its own input.
3. **One reader in the browser**, and chips resolve from it. Delete
   `/api/entities`.
4. **Facts from the record go on the rows**: needs re-run, the summary
   file, the Ingest label. Delete `rerender` and the stopgap.
5. **Measure on a `cp -c` clone of a real library**, before and after.
   Record how long the page takes to open, the requests per minute
   during a sync, and the `manage_rows` slow lines.

## Questions to answer before step 2

- **How often does `StorePart::Metrics` move during a sync?** If it is
  every progress tick, reading the current run's metrics, even through
  the index, may want its own one-a-second limit.
- **Does `runs.count_problem_lines_per_step` (about 180 ms) belong in
  the table at all?** It counts warn and error lines in the run log per
  step. That count could be a metric the runner writes as it logs.
- **Should the held table be pushed on the stream instead of fetched?**
  The stream carries no payloads today (`watch.rs`, `RootEvent`), and a
  fetch of held bytes is cheap, so the plan keeps fetching.

## What a person notices

- The Sources page opens at once, because nothing is read to open it.
- During a sync the app stops re-reading the same data several times a
  second. The server does about one rebuild a second, at most, instead
  of two to six.
- A running source says how far it has got ("Running · 2/3 complete").
- A chip, the dashboard and the table always agree, because they draw
  the same copy of the table.
- A store that cannot be read is named in a banner instead of showing
  "Never run" or an empty table.
- The "to re-run" pill and the upgrade screen go away as soon as the
  re-run finishes.
