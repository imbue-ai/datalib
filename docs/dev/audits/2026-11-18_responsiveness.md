# Audit: why the app feels slow, stale and opaque, 2026-11-18

A record, not reference. It reads the tree at `a8582fb8` for what makes
the UI feel unresponsive or out of date, the search slow, and a sync's
progress hard to see. Line numbers are as of that commit. Read the code
before repeating any claim here as current.

**How it was read.** Four read-only passes ran side by side: how the UI
learns of a change and fetches; the path of a search from the box to the
SQL and back; how progress is made, carried and shown; and how the sync
loop schedules work. Each pass's strongest claims were then re-read in
the code by hand. Two were dropped or narrowed on that re-read (end of
this file). Then the app was run on the TNG fixture (`:dev_tng`) and
driven with Playwright. The fixture has 178 documents and no raw stores,
so it shows what a request costs in round trips, not what it costs at
scale, and no sync could run on it. A sync's index commits were stood in
for by raising the grid index's `store_generation` by hand, five times
1.5 s apart. Costs "at 2M rows" are extrapolated from a synthetic
300k-row index built with the DDL from the code, not measured on a
library.

**How far to trust it.** "Measured" means watched in the running app.
"Verified" means the code path was read end to end. "Inferred" means the
effect on a person follows from the code but was not seen.

## What is already sound

The bones are good, and most of the jank comes from a few places that
undo them:

- **One push channel, and no polling for data.** The page holds one SSE
  connection (`ui/src/live.ts`). Its frames carry no data and only say
  "ask again". The server debounces them (300 ms) and throttles the busy
  ones (1 s). Measured: 30 s idle on the search view made **0** requests.
- **Off-screen cards wait.** A card in a hidden tab holds its frames
  until it is shown (`holdWhileOffScreen`). Measured: `manage.rows`
  frames arrived while the Search tab was showing, and no
  `/api/manage/rows` was fetched.
- **"Sync now" starts at once.** No tick sits between the click and the
  step starting. A commit to the supervisor store wakes the loop through
  a FIFO.
- **A search replaces the one before it.** Measured: two searches 50 ms
  apart ended showing the second.
- **Small payloads, few round trips.** On TNG, a search took 57–150 ms
  from Enter to rows on screen, and the dashboard's first paint needed
  about 20 requests, none slower than 60 ms.
- **The ETA math is careful.** It measures for 15 s before showing a
  figure, and says "stalled", "growing" or "flat" rather than invent one
  (`http/src/manage/queue.rs`).

## Fix these first

Ranked by how much a person feels the problem, then by how cheap the fix
is.

### 1. The search grid blurs and ignores clicks on every index commit (measured)

`GridCard.ce.vue` refreshes on `index_changed` through the same path a
new search takes (`:1502-1513`). That path sets `loading.value = true`
(`:966`, `:1118`), and `.grid--loading` is `filter: blur(2px);
pointer-events: none` with a "searching…" overlay (`:3022`).

- **Measured:** five index commits gave five blurred spans, about 925 ms
  in all on TNG, with 178 rows.
- **On a large library:** each refresh also re-reads every row the grid
  already holds, up to 100,000 (`grid/pagedWindow.ts:113`), and diffs
  them on the main thread (`rowPatch.ts`). So the blur lasts longer on a
  big library, and a sync commits often.
- **What a person notices:** while a sync runs, the grid flickers, and a
  click or double-click sometimes does nothing.
- **Fix (small):** don't set `loading` for a refresh, show at most a
  quiet "updating" mark, and space refreshes about 2 s apart.

### 2. Every read of the grid index waits for one connection (verified)

The applet reads the grid index through `datalib_pin::open_reader`, a
`max_connections(1)` pool (`pin/src/lib.rs:49`), held behind
`DoltRepo::pinned` (`unified_grid_and_search_index/src/dolt_repo.rs:490`).

- **What waits in that queue:** every `/search`, `/search/groups`,
  `/search/values` (autocomplete), `/chat`, `/children`, `/people` and
  `/problems` from every card. One `/search` takes the connection five
  or six times in a row.
- **Statements are not interrupted:** a request the browser gave up on
  still finishes its statement, because nothing calls
  `sqlite3_interrupt`. So a slow sort or grouping delays everything
  behind it.
- **Why one connection is no longer needed:** it was the rule for
  doltlite. The grid index is now plain SQLite in WAL mode
  (`etl/render/src/grid_index.rs`), where several readers are safe.
- **Fix (small):** a pool of 4–8 read-only connections, one transaction
  per request, `mmap_size` and a larger `cache_size` on each, and
  `sqlite3_interrupt` when a request is dropped.

### 3. The Manage rows are rebuilt from nothing on every request (verified; part measured)

`GET /api/manage/rows` (`http/src/manage/mod.rs:342-400`,
`lib.rs:1463-1528`) has no cache. Each answer:

- re-reads and re-checks the config;
- loads the loop's whole record;
- opens `runs.sqlite` seven times, and each `latest_metric` sorts every
  sample of its metric across all kept runs (`runs/src/store.rs:603`);
- lists every step's folder and re-reads the problems stores that moved.

`POST /api/entities`, which the chips use, rebuilds all of it again
(`manage/mod.rs:436`).

- **Measured:** five index commits gave five `POST /api/entities` with
  the Search tab showing.
- **During a real sync:** each running step reports its memory every
  2 s (`dag/src/memory.rs:29`), so the run store always has something
  new, and `manage.rows` fires about once a second the whole time.
- **Earlier measurement:** the 10-29 audit counted 194 of 359 answers
  over 1 s, and one at 28 s. Part of that was a WAL that has since been
  fixed, so re-measure before trusting the size of the problem.
- **What a person notices:** the Manage table, the dashboard and the
  toolbar pill lag a step's real state by seconds.
- **Fix (medium):** keep the built answer in memory and drop it when the
  watcher emits `manage.rows`. Answer entities from the same copy. Keep
  one runs-store pool, and cache the config check by the file's mtime.

### 4. One new email re-runs every source's embed, and a long embed keeps new data out of the grid (verified)

- **Every embed re-runs:** the index step claims one version for its
  whole file: the grid generation plus the search generation
  (`datalib_step/src/unified_grid_and_search_index.rs:274`). Each
  source's `embed` versions itself by hashing what it read
  (`index_steps.rs:162-182`), so any change to any source makes every
  embed stale.
- **The map re-runs too:** `embedding_map` lays out every vector again,
  with no "nothing changed" exit (`embedding_map.rs`).
- **A long embed holds the grid back:** the embeds read the index off
  disk, so the runner makes the index step wait while any embed runs
  (`supervisor/tick.rs:321-327`; the comment at
  `unified_grid_and_search_index.rs:195`).
- **What a person notices:** during a long first embed of one source, an
  email that has already downloaded and rendered does not reach the
  grid until that embed ends. The 10-29 audit saw a 27-minute one. A
  source that just synced also waits behind N−1 embeds that find
  nothing to do.
- **Fix (medium):**
  - give the index a version per source, so a source's embed reads only
    its own;
  - let the map skip when the set of vectors is unchanged;
  - let an embed read a snapshot of its document list and let go of the
    file, so the index can write while the embed runs.

### 5. A search builds its whole result list, and a header sort cannot use an index (verified, timed on synthetic data)

- **The whole list is built:** `ordered_sql_where` has no `LIMIT`
  (`dolt_repo.rs:286`). Every new search, and every refresh after an
  index commit, reads every matching uuid and turns each into an `Entry`
  of about 200 B (`applets/.../mod.rs:989`). The cache keeps 16 such
  lists. Each index generation is a cache miss, so every grid on screen
  rebuilds its list.
- **Header sorts scan:** `order_by` emits `NULLIF(col,'') IS NULL, col
  COLLATE NOCASE` (`sort.rs:68-89`), which no index matches. `EXPLAIN`
  shows `USE TEMP B-TREE FOR ORDER BY`.
- **Synthetic timings:** 105 ms (default order) and 369 ms (header sort)
  at 300k rows; about 0.7 s and 2.5 s at 2M by extrapolation.
- **Fix (medium):**
  - page the default order by key (`WHERE (touched_at_utc, is_document,
    uuid) < (?,?,?) LIMIT n`) and take the total from `count(*)`;
  - store blanks as NULL and declare the sortable text columns `COLLATE
    NOCASE`, so a sort matches an index;
  - on refresh, re-read only the rows in view and the newest page
    (`pagedWindow.ts`).

### 6. Keyword results wait for search by meaning (verified)

`fused_ranking` runs `tokio::join!(fields, words, meaning)` (`mod.rs:1204`)
and returns nothing until all three are done.

- **The query is embedded every time:** there is no cache of query
  embeddings, so each refresh after an index commit embeds it again.
- **The first search can wait 20 s:** a search before the model is ready
  waits up to `EMBED_DEADLINE` (20 s).
- **Vector scoring blocks the executor:** `HeldVectors::nearest` scores
  every vector in a loop on the tokio executor rather than in
  `spawn_blocking`, and it computes a norm it then discards
  (`search_index.rs:200-235`).
- **Fix:**
  - cache query embeddings, small;
  - use a plain dot product (stored vectors are unit length) in
    `spawn_blocking`, small;
  - answer with keyword hits first and fuse in the meaning results when
    they arrive, medium.

### 7. Progress freezes or reads wrong exactly when a person is waiting on it (verified)

- **The toolbar's "N min left" freezes.**
  - *Cause:* `SyncProgressChrome.vue:90-93` reloads only on `dag`
    frames, and a `dag` frame comes only when a step changes state.
  - *What a person notices:* during a 40-minute backfill the menu keeps
    the ETA it had when the download started. The dashboard's head line
    says only "Syncing now" (`useDashboard.ts:102`).
  - *Fix (small):* also reload on `manage.rows`.
- **A quiet step never reads "stalled".**
  - *Cause:* the stall check runs only when `/api/manage/rows` is
    fetched (`queue.rs:281`), and nothing fetches it without a frame. A
    step that writes no metric (a commit, a gc, a model load, the index's
    last phases) sends no frame.
  - *What a person notices:* its row keeps its last "3m left"
    indefinitely.
  - *Fix (small):* refetch every 10–15 s while any row is running.
- **The index step's queue reads 0 after its first source.**
  - *Cause:* `datalib_step/src/search_index.rs:217` sets a new length
    per source, but `done` keeps counting across the whole pass, and
    `queued = total − done` saturates at 0 (`dag/src/runs_sink.rs:378`).
    This is the "one bar per step" trap that `dag/README.md` warns
    about.
  - *Fix (small):* one bar across all sources.
- **The embed step has the same two-bar problem** (`:388` cutting,
  `:485` embedding). Its model download, hundreds of MB, reports nothing
  while the message says "cutting chunks".
- **A render has no total** (`datalib_step/src/render.rs:285,349`).
  - *What a person notices:* a full re-render of a 100k-document source
    shows a spinner for many minutes.
  - *Fix (medium):* announce the stale buckets, or the raw row count, as
    the length.
- **A source's ETA covers only the step running now** (`queue.rs:242,452`).
  - *What a person notices:* "2 min left", followed by a 27-minute
    embed.
  - *Fix:* say it is the current step's ETA, or add the last durations
    of the steps still to come.
- **Progress that has a total but is sent as text:**
  - the grid index load ("loaded i/N", `grid_index.rs:1798`);
  - file hashing ("3 GB of 50 GB", `file_changes_scan.rs:1195`).

  Neither sends `set_length` or `inc`, so neither gets a bar, an ETA or
  a stall note. The text itself shows only on hover in the Sources card
  (`SourcesCard.ce.vue:110`).
- **The migrate pass after an upgrade shows "Updating… 3 of 17" and
  nothing more** (`ui/src/pass.ts:91,102`).
  - It runs its steps one at a time on every new build
    (`http/src/supervisor.rs:383-410`).
  - While it runs, no row shows that step's own message
    (`lib.rs:1541-1544`).

### 8. Smaller fixes, each a person would notice

| what | where | fix |
|---|---|---|
| A Sync click while other sources' embeds or the map still run returns the old open request; nothing starts and nothing says why | `http/src/lib.rs:1823-1835` | match only requests whose roots have not started yet |
| Free disk space, reset and purge run inside the loop's body: rows stay "Running", Stop is not heard, and `POST /api/requests` gives up after 3 s | `supervisor/round.rs:423-447, 933-1017` | run the wipe as a task beside the steps (medium) |
| The whole data root is measured every 5 s for the whole busy period, embeds and map included | `http/src/usage.rs:37, 549-554` | measure only the running steps' trees, and space it out over a long run |
| Returning to the browser tab resyncs everything, which includes a full walk of the data root; opening the Sources card waits for one | `live.ts:253-266`; `sourcesCardModel.ts:1141` | draw cached sizes first and walk only if the last walk is old (`SourceCard.ce.vue:368` already does this) |
| The search grid is the only live subscriber with no `resync`, so after sleep or a server restart it shows old rows until the next commit | `GridCard.ce.vue:1506-1513` | `resync: refreshRows` |
| Reloads on `dag` frames have no guard and no sequence check, so they pile up behind slow answers and an older one can overwrite a newer | `SyncProgressChrome.vue:90`, `HistoryCard.ce.vue:335`, `libs/sourceDagView.ts:283`, `TableCard.ce.vue:57` | `newestAnswer(…, {spacing})` |
| The dashboard's Sync buttons show nothing until the round trip ends, so people click twice | `useDashboard.ts:108-123`, `DashboardSyncBar.ce.vue` | say "Queued…" at once, as `rowActions.ts:300` does |
| Background reloads raise error toasts that come back every 5 s while the first is still on screen | `toasts.ts:26-41`, `api.ts:591` | don't repeat a toast still showing; `toast:false` for live reloads |
| Holding an arrow key in List + preview rebuilds the preview from nothing on each press, with no debounce or abort | `SearchList.ce.vue:231-252` | debounce about 120 ms, keep the old preview until the new one is ready, pass a signal |
| Autocomplete asks on every keystroke with no abort, and each ask queues on the one connection (2) | `search/field.ts:450` | pass the signal, debounce, cache top values per generation |
| Download checkpoints double their interval: in a six-hour backfill new rows reach the grid at about 2 h and 4 h | `etl/src/checkpointer.rs` | cap the interval, for example at 5 minutes |
| A rate-limited step is re-invoked in full after 1 s, then 2 s | `dag/src/scheduler.rs:26-55` | back off by minutes, or honour a Retry-After the step reports |
| "Now" is pinned once per busy period, so a source synced three hours into it uses a three-hour-old now for its windows | `http/src/supervisor.rs:532-547` | pin it per request |
| Four download slots, given out in config order | `supervisor/locks.rs:65-92` | order by when the request opened |
| The desktop app shows disabled buttons, then a blank page, while the server boots | `tauri/launcher-dist/index.html:505`, `http/src/main.rs:187-201` | an "Opening…" state, and an early loading page |

### 9. Two things that are wrong results, not slowness

- **A filtered search can drop name and id matches** (verified).
  - *Cause:* when other terms narrow the search, the name/id ranking is
    fetched unbounded (`mod.rs:1160-1185`), cut to its best 1,000
    (`:1232`), and only then filtered (`entries_kept`, `:1086`).
  - *What a person notices:* `from:ann meeting` keeps whichever of the
    library-wide best 1,000 "meeting" name matches are Ann's, which may
    be none.
  - *Fix:* filter inside the query, or cut after filtering.
- **The index step's queue reads 0** (item 7) is a wrong number, not
  only a stale one.

## Search costs that grow with the library

Smaller than 5 and 6, all verified in code and timed on the synthetic
index:

- **Showing a page of free-text hits reads whole documents.**
  `stored_text::spell` reads each hit document in full from its render
  store (`mod.rs:876-967`). Long agent sessions are megabytes each. The
  section's byte range (`search_sections.start_byte`/`end_byte`) is
  already stored, so only that part needs reading.
- **The count of documents without vectors scans the grid after every
  new free-text search** (`documents_with_pages`, `dolt_repo.rs:1469`).
  It took 209 ms at 300k rows. `search_documents`, about 1/20 the size,
  can answer it.
- **`from:*` and `recipient:*` scan `terms`** (`db.rs:202`). That took
  1.5 s at 600k terms. An index on `terms(kind, row_id)` fixes it.
- **Broad negative filters build large sets.** `-source:x` and
  `after:2015` load every matching row into hash sets, and send them as
  JSON to each FTS query (`mod.rs:1483-1511`).
- **No `ANALYZE` or `PRAGMA optimize` is ever run.** The planner is
  steered with `likelihood()` and `INDEXED BY` instead.

## Measured on TNG

The probe scripts were kept outside the tree.

| what | result |
|---|---|
| dashboard first load | about 20 requests, settled at 1.2 s; heaviest `/api/manage/rows` 162 KB at 49 ms; `/api/entities` 55 KB at 54 ms |
| search, Enter to rows on screen | 57–150 ms (`from:picard tea` slowest) |
| two searches 50 ms apart | the second one shown |
| 30 s idle on the search view | 0 requests |
| 5 index commits, table view | 5 `/search`, 5 `POST /api/entities`, 5 `index_state`; 5 blurred spans, about 925 ms |
| 5 index commits, List + preview | the same requests; the open document was **not** re-fetched (see below) |

## Dropped or narrowed on re-reading

- **"`runs.sqlite`'s WAL is never checkpointed."** It is fixed: a
  doltlite upgrade restored SQLite's automatic checkpoint, and
  `the_wal_stays_within_its_limit_by_itself` (`runs/src/store.rs`) holds
  it.
- **"The document card re-fetches the whole document on every
  `index_changed`."** `DocCard.ce.vue:596-610` does subscribe so. But
  the List + preview pane did not re-fetch in the run above, so it is
  not the card drawing that preview. Whether a document opened as its
  own card does was not driven. The same file's double render
  (`DocCard.ce.vue:233` renders the body to collect image URLs;
  `ChatBody.ce.vue:139` renders it again) was read in the code and not
  timed.

## Not checked

- Anything at real scale: no sync ran, and the large-library figures
  above are extrapolated.
- Slack's file fetches one at a time (10-29 audit, finding 1) and other
  provider-side download speed.
- The CLI's terminal progress bars.
