# Paged grids: the search grid and the log card load a page, then more

*Proposal (2026-09-25); steps 1 to 3 of "Order of work" are built, and
the server half of step 4; the rest is not. Every number was measured
on 2026-09-25 against a copy of `~/datalib/stay_alive_1` (74,023
`grid_rows`, a 1.3 GB index, 238,716 log lines) with the
`datalib-doltlite` shell; each measurement includes about 0.1 s of
process start.*

## What is slow, measured

Opening the search grid on that root took **9.5 s**
(`GET /applet/unified_index/search?q=&limit=100000`, 06:37:56 in its
`runs.sqlite`). On 2026-09-23 the Browse cards' `source_id:… is:document`
searches took 4–12 s each, and `qmd_state` took up to 5 s.

There are three causes, and paging fixes only the first of them by itself.

1. **The grid asks for everything.** `GridCard.ce.vue` sends
   `limit=100000` (`SEARCH_LIMIT`), and every sort, group, header filter
   and count happens in the browser over the full set. A full
   `SELECT *` over `grid_rows` takes 28 s and returns 384 MB, 330 MB of
   which is the `text` column.
2. **The order the grid wants has no index, and the pinned read
   could not use one anyway.** `grid_rows` has only its primary key.
   Newest-first (`ORDER BY coalesce(modified_at_utc, created_at_utc)
   DESC LIMIT 200`) takes 5.6 s. With an index it takes **0.26 s**, but
   only on the plain table. The applet pins its reads through
   `dolt_at_grid_rows('<hash>')`, a virtual table that ignores
   secondary indexes: its plan is `SCAN … VIRTUAL TABLE` plus
   `USE TEMP B-TREE FOR ORDER BY` even when the index exists. Through
   it, `SELECT uuid … ORDER BY … LIMIT 200` still takes 2.5 s. How to
   pin *and* use the index is below, under "Pinned and indexed".
3. **An ordered scan with a selective filter is worse than no index.**
   `provider='garmin'` (one row) with the sort index takes **38 s**:
   SQLite walks the whole index in order and checks every row. `ANALYZE`
   does not change the plan (29 s). A composite index
   `(provider, sort key, uuid)` takes it to **0.08 s**, and the matching
   `count(*)` is 0.08 s too.

The log card is fast but opens at the wrong end. `/api/log` is
`ORDER BY seq LIMIT 5000` with a cursor that only moves forward
(`after_seq`), so a fresh card shows the *oldest* 5000 lines and never
the newest ones. Newest-first costs nothing on `runs.sqlite`: 500 lines
take under 10 ms, or 60 ms filtered by level.

## The shape: one window over a time-ordered list

Both grids show a list ordered by time, open at its newest end, and
grow in both directions:

| | opens at | scrolling away from newest | a live frame |
|---|---|---|---|
| search grid | newest row at the **top** | fetch older rows, below | fetch rows newer than the top |
| log card | newest line at the **bottom** | fetch older lines, above | fetch lines newer than the bottom (the tail, today) |

They are the same machine with the display flipped. That is the
shared code path.

**The contract.** A paged endpoint takes the query and a cursor, and
returns rows plus the cursor of the next page and what the page was
read at:

```
search grid:  q, sort, limit, offset          (no offset = the first page)
              -> rows, total, next_offset|null, at: <commit>
log card:     q, limit, before=<seq>|after=<seq>  (neither = the newest page)
              -> rows (newest first), older|null, newer|null, at: <seq>
```

The two cursors differ because the two lists do. A search's list is
built once per commit and held (below), so a position in it is stable:
rows landing at the newest end make a new commit, a new `at` and a new
list, never a shifted page. The log has no commit to hold, so its
cursor is the last line's `seq`, *keyset* paging (`WHERE seq < ? ORDER
BY seq DESC LIMIT n`), which does not shift when lines are appended
while someone scrolls.

**The UI half** is one module, `ui/src/grid/pagedWindow.ts`, split the
way `docs/dev/style.md` asks:

- a pure core: given the window it holds (rows, the two cursors, what
  is in flight) and the viewport, decide the next fetch, or none; given
  a response, compute the new window. No SlickGrid, no `fetch`, so it
  is unit-tested as plain values.
- a thin shell per card that feeds it viewport events, runs the fetch
  and applies the result to the DataView. It reuses `redrawChanged`
  and `keepActiveOnRecord`, plus the anchor-row scroll that
  `applyPatch` already does, so a page prepended above the viewport
  does not move what the user is looking at.

`GridCard`, `RunLogPanel` and, later, `TableGrid` each plug a fetcher
into it. `cards.md` warns against a wrapper around the grid; this is a
data source beside it, not a wrapper.

slickgrid-universal has its own infinite scroll, driven through
`backendServiceApi`. As far as I know it is page-number based and
knows nothing of a tail, so the proposal is our own small module.
Check that before writing it, because I did not read its source.

## Server: the search grid

**The default order is newest first by a new `touched_at` column:**
when the record last changed at its source. The builder sets it to
`modified_at`, else `created_at`, and a provider sets it itself only
when the record's last change is neither. Filling `modified_at` from
`created_at` instead was the first idea, and it would have broken two
things. `modified_at`'s documented null means "never known to have
changed", and a calendar event's `created_at` is when it *happens*. On
the measured root 936 events are dated in the future, up to 2031, so
newest-first by modified-else-created would have opened the grid on
several pages of 2031 holidays. A calendar row's `touched_at` is the
event's edit stamp instead. `touched_at_utc`, like the other `_utc`
twins, is derived at index time, so the sort and its index are one
plain column. (An expression index is used only when every query
repeats the expression exactly.)

**`source_id` is a derived column** holding the group id: the path's
first segment, or `datalib` for a storage row
(`GridRow::derived_source_id`). The `source_id:` filter was
`INSTR(qmd_path, 'gmail/') = 1`, which no index can serve, and it is
the filter every Browse card uses. It is now `source_id = ?`.

**Indexes** go through a new `index = "name:cols"` on `PortableTable`,
emitted apart from the table DDL so only the unified index creates
them; every render store also has a `grid_rows` and does not pay:

- `(touched_at_utc, is_document, uuid)` for the unfiltered grid, a
  document ahead of its rows at the same moment;
- `(col, touched_at_utc, is_document, uuid)` for each key the search
  bar filters on: `source_id`, `source_label`, `kind`,
  `channel`, `conversation_uuid`, `author`, `account`, `project`,
  `notion_page_uuid`, `diff_status`, and `(is_document, touched_at_utc,
  uuid)`.

`every_filter_key_is_served_by_an_index` requires each key's query plan
to *search* an index on its own column. Scanning the newest-first index
and testing every row also avoids a sort, and is the 38 s walk above;
the test's first version accepted that, and passed with an index
deleted, before it was tightened. It also fails on an index no key
plans with: step 2 shipped one on `provider`, which the search bar has
no key for, and step 3 dropped it. `before:`/`after:` filter on
`created_at_utc` and have no index: a `before:` far back still walks.

**Every column stays sortable, and every page takes one path.** The
first page of a search lists every row it holds, as uuids in order:
`SELECT uuid … WHERE <filter> ORDER BY <sort>, uuid`. The list goes in
the result cache, and every page, the first included, is a slice of it
plus a lookup of those rows by uuid. The cursor is a position in the
list, and the commit it was listed at.

A second path was planned: keyset pages straight off an index when the
index gives the order, with the cursor the sort key plus `uuid`. Timed
at full size (74,163 rows, net of about 0.15 s of process start, in a
read transaction, 2026-09-26), listing is too cheap to be worth it:

| listing every uuid | time |
|---|---|
| newest first, no filter (the index's order) | about 0.05 s |
| newest first, the largest source (46,661 rows) | about 0.03 s |
| by `author`, which no index orders | about 0.5 s |
| then one 200-row page, by uuid | under 0.01 s |

One path means one cursor, a `total` that is simply the list's length,
and no second set of tests. (The 5 s once quoted for an unindexed sort
was read through `dolt_at_`, which uses no index at all.)

**The result cache** lives in the applet
(`applets/src/unified_index/results.rs`). It is keyed by
`(q, sort, commit)` and holds the last 16 ordered uuid lists, each with
qmd's score and matched words for a free-text search. A list for all
74k rows is about 3 MB. An entry for an old commit is simply never
asked for again once the UI moves to the new one. Grouping (step 5)
adds its group lists and counts to the same cache.

**Drag-to-group stays, and moves to the server.** Grouping by a column
is two kinds of request:

- **The group list:** `SELECT <col>, count(*) … WHERE <filter> GROUP BY
  <col>`. Every filterable column has a `(col, touched_at_utc, …)`
  index, so this is an index-only scan: the equivalent `count(*)` for
  one provider took 0.08 s. A column without an index scans once and
  lands in the result cache. The counts in the group headers are true
  counts for the whole result, not for what happens to be loaded.
- **An expanded group** is its own paged window over the same query
  with `<col> = <value>` added. That is exactly an indexed filter, so
  its rows come newest-first off the composite index, and each group
  scrolls and loads more on its own. Nested grouping repeats the
  pattern one level down, with the outer group's value added to the
  filter.

In the browser, the draggable-grouping drop zone stays the control.
The grid stops handing grouping to SlickGrid's DataView, which needs
every row, and builds the flat list itself: group header rows, the
loaded rows of each expanded group, and a "loading…" row at the end of
a group that has more. The log card groups the same way over
`runs.sqlite`. `log_by_run_step` already serves step and run; level
and target would each want an index.

**The indexes live on `main`, and the writer pays for them.** Keeping
them only on the read branch, so the writer never pays, looked better
and measured worse. Measured on a copy of `stay_alive_1`'s store, with
13 indexes:

| | 1,000-row seal | 10,000-row seal |
|---|---|---|
| writer seal, no extra indexes | 0.28 s | 0.49 s |
| writer seal, all 13 indexes | 0.19 s | 1.11 s |
| `reader` fast-forwarding to that seal | 0.05 s | — |
| `reader` merging that seal, indexes only on `reader` | 10.56 s | 10.62 s |

A branch that differs from `main` can no longer fast-forward, so every
update is a three-way merge. On a 74k-row table that costs about 2.5 s
even when the branch only differs by an unrelated table. Each extra
index on the branch then adds roughly half a second per merge,
whatever the size of the change. The merge probably re-derives each
index rather than applying the diff. The writer, maintaining the same
13 indexes, pays about 0.6 s extra on a 10k-row seal. Building all 13
from nothing over 74k rows took 8.5 s, which bounds what a full
re-index pays.

So paying later costs more than paying now, and the grid would lag
each seal by ~10 s. And keeping indexes on a branch of the reader's own
needs that reader to write the branch, which a read transaction does
not (below). A real sync's seals have not been timed with the indexes
yet; the next full sync of a real root will say, since `runs.sqlite`
keeps every `grid_index` run's duration and the rebuilds from before
this change are there to compare with.

It is a shape change to derived stores only, so nothing has to migrate
by hand: every source's render store sees its `grid_rows` DDL change
and re-renders once, and the unified index drops and rebuilds on the
drift (`init_schema`).

**Pinned and indexed: a read transaction is the snapshot.** The
applet has to read one commit and use the indexes. `dolt_at_` cannot do
the second. Doltlite's `atBestIndex` seeks only on a rowid alias
(`INTEGER PRIMARY KEY`), and `grid_rows` is keyed by a text `uuid`, so
even `WHERE uuid = ?` at a pinned commit scans the table (0.9 s). It
never offers secondary indexes and never consumes `ORDER BY`.

What does work, measured with two processes on a small store (one
reader, and a writer publishing the way `commit_run` does: commit on
`datalib_writer`, then `dolt_branch('-f', 'main', …)`):

- **A `BEGIN` on a read-only connection on `main` holds one commit.**
  Five publishes landed while it was open. Inside it, `count(*)` stayed
  where it was and `dolt_hashof('HEAD')` kept naming the commit the
  transaction started at. After `COMMIT`, the next `BEGIN` read the
  newest commit.
- **Plain tables inside it use their indexes**
  (`SEARCH t USING COVERING INDEX`).
- **It does not block the writer.** Each publish took 0.02 s with the
  read transaction open.
- **It sees only published work.** The writer is on its own branch,
  which `writer_branches.md` measured too.

So **the applet's snapshot is a read transaction.** It holds one
dedicated read-only connection out of the pool with a transaction open
on it, and `COMMIT; BEGIN` is how it moves to the newest `main`.

**Measured at full size (step 1).** `hack/read_transaction_at_scale/`
ran a writer sealing 40 times against a copy of `stay_alive_1`'s index
(1.3 GB, 74k rows), each seal changing 1,000 rows through `commit_run`.
It ran once with the writer alone and once beside a read-only connection
holding transactions:

| 40 seals | writer alone | beside 200 ms transactions | beside one transaction held across every seal |
|---|---|---|---|
| seals refused | 0 | 0 | 0, 0, 0 (three runs) |
| median seal | 842 ms | 840 ms | 871, 828, 1,091 ms (alone, back to back: 832, 1,131 ms) |
| file growth | 572.8 MB | 572.8 MB | 572.8 MB |

The reader added nothing to the file, and every transaction read
exactly one commit, including the one held for all 40 seals. One
long-hold run had a stretch of seals at 2.5–10.8 s. It recovered while
the transaction was still open, and did not repeat in two back-to-back
reruns. In the second of those, the load average reached 8.4 and the
writer *alone* hit 3 s. So the stretch is the machine, not the reader. `doltlite_two_process_test`'s
`a_held_read_transaction_is_a_snapshot_while_the_writer_seals` keeps
the property: 500 seals flat out, no seal refused, every transaction
one commit. Nothing runs `dolt_gc` on the index (only `fsindex` and
`sqlite_mirror` call it), so compaction cannot pull chunks out from
under an open snapshot.

**A read branch was tried and rejected.** The alternative was a branch
of the applet's own, `reader`, fast-forwarded with `dolt_merge('main')`
from a read-write connection on it, with every query on read-only
connections on that branch. It would have let any number of connections
share one named snapshot. But the merging connection is a second writer
on the file, and the real writer pays for it. Run through the real
`commit_run` seal, 300 seals flat out, three runs each:

| reader beside the writer | writer refused |
|---|---|
| read branch, merging every 20 ms | 7, **hung forever**, 9 |
| read branch, merging every 100 ms | 1, 2, 1 |
| held read transaction | 0, 0, 0 |

A refused seal fails with `commit conflict: another connection
committed to this branch` from `dolt_commit`, or `database is locked`
from moving `main`. That is a failed sync step. Once the writer blocked
in doltlite's file lock and never came back. Merging flat out also left
the read branch with a dirty working set ("uncommitted changes — commit
or reset before merging"). An earlier probe through the `doltlite` shell
had shown the writer never refused. It was wrong, because the shell's
writer was not `commit_run`'s commit-then-publish. This is
`etl/README.md` § "A branch each is not a way around the one-writer
rule" again, now from a writer of one ref. (Keeping the indexes only on
such a branch was also measured, and was slower anyway; see "The
indexes live on `main`".)

The applet answers each request with the commit its transaction read,
as `at`. When that changes, the UI treats it like `index_changed` and
re-reads the key range it holds. The `pinned_<table>` views and their
catalog races do not apply: the snapshot includes the catalog.

This goes against a rule in `AGENTS.md` ("a reader … pins a commit"
through `dolt_at_`), so step 2 changes the rule's text: a reader pins
a commit with `dolt_at_` *or* a held read transaction, and the second is
for a reader that needs indexes. Passes that already use `dolt_at_` keep it.

Worth filing upstream anyway (dolthub/doltlite): `dolt_at_` seeking on
a non-integer primary key, which would at least make pinned lookups by
uuid cheap.

**The search endpoint** takes `offset` and `sort` (`created_at:desc`;
a column the grid shows, or `score`), with a default `limit` of 200.
It answers with `total` (every row the search holds; this replaced
`total_estimated`), `next_offset` (null on the last page) and `at`. The
cursor and `at` are in the body because the gateway drops a response's
headers. The other `/search` callers (dactal's 2000, perseus's 200000,
`bridge.js`, the e2e specs) keep working unchanged: no offset is the
first page of `limit` rows.

**Counting** needs no request of its own: the list is already built for
the first page, and `total` is its length.

**Free text via qmd** ranks up to 1,000 hits once per search. The
structured terms then filter that ranking in one statement
(`filter_uuids`), which keeps qmd's order unless a sort replaces it;
`score:asc` reads the ranking from the bottom. The list is paged out
of the result cache like any other, rather than asking qmd again per
page. The old path cut qmd's hits to the page size *before* the
structured filter, so a filtered free-text search could come back short
while matching rows sat further down the ranking. Separately, every
free-text search still loads all of `grid_rows` through
`grid_row_refs()` to map hits to rows; that becomes a lookup by
`qmd_path` over an index.

## Server: the log

`/api/log` gains `before_seq` and newest-first order: `ORDER BY seq
DESC LIMIT ?`, with the page reversed for display. The card opens on the
newest 500 lines. Tailing stays `after_seq`, but it loops until caught
up instead of taking one 5000-line page per frame. No schema change.
Free-text `msg` search scans (4 s for a word that matches nothing in
238k lines), which is tolerable for now. FTS5 would be the fix if it
ever matters.

## What the grid does in the browser today, and where each goes

Everything below assumes the full row set is loaded, so each needs a
decision:

| today, in the browser | proposal |
|---|---|
| sort by any column header | server-side, every column: off an index when one gives the order, otherwise sorted once into the result cache and paged from there |
| drag a column to group, with counts | server-side: the group list with true counts, then each expanded group as its own paged window (above) |
| the header filter row (per-column text boxes) | becomes query tokens, as keep/exclude already are |
| adaptive column hiding | computed from the first page |
| restore the selected row from the URL | seek to it: a page on each side of its key, the same cursor machinery |
| refetch everything on `index_changed` and diff | re-read the key range the window holds (the sort key `BETWEEN` the oldest and newest held) plus anything newer, and patch that |
| `qmd_state` for every row in the result | only the loaded rows (see below) |
| `__fwGridApi.rows()` in e2e | means loaded rows; specs that count everything use the count endpoint |

## `qmd_state`, separately

The endpoint behind the Indexed / Embedded columns. Measured on the
same root's qmd index (18,711 documents, 46,475 vectors, a copy):

| cost, per call | before | after (step 3) |
|---|---|---|
| `summary()`, the "N of M documents searchable" totals | 0.41 s, on every call, columns shown or not | once per change to the index files (`SummaryCache`, keyed on the size and time of `index.sqlite` and its `-wal`) |
| vector counts, per batch of 400 hashes | 0.04 s: every vector in the index grouped, once per batch | 0.01 s: only the batch's own vectors |
| documents asked about | every one behind the result set, up to the 2,000 cap and a toast past it; 2,000 files read and hashed is about 0.35 s | the ones behind the rows on screen and 50 either side, asked again as the grid scrolls (`ui/src/grid/qmdAsk.ts`) |

The 1.5–5 s calls in the logs overlapped 4–12 s searches, and waited
behind them on the applet's one connection to the grid index; step 2
made those searches fast.

## Every branch earns its keep

Each step's PR ends with a coverage run over the code it added. Every
uncovered branch is either deleted or given a test, and the PR says
which. A branch nobody can reach from a test is usually one nobody
needs.

- **Rust:** `tools/run_coverage.sh` (`docs/dev/coverage.md`) over the
  tests the step touched, with any `rust_binary` those tests launch
  after `--`. It measures LLVM regions, where each arm of a `match` or
  `if` is its own region. True branch counts need
  `-Zcoverage-options=branch`, which has been nightly-only; check the
  pinned toolchain before relying on it, and use regions if not.
- **UI:** `pagedWindow.ts`'s pure core, and whatever else a step adds
  under `ui/src/grid/`, runs under Vitest with `@vitest/coverage-v8`,
  which reports branches. That is a new dev dependency (MIT), added in
  step 4.

## Order of work

Each step is one PR, useful on its own:

1. **Done: the read transaction, at full size.** The held-transaction
   scenario in `doltlite_two_process_test`, the full-size harness, and
   the measurements above. The read branch was rejected.
2. **Done: `touched_at`, `source_id` and the indexes, and the applet
   reading in a read transaction.** Default order newest first. The UI
   is unchanged, so it still asks for everything, but the query stops
   scanning to sort.
3. **Done: `qmd_state` fixes**, and the unused `provider` index
   dropped.
4. **The page contract.**
   - **4a, done: the server.** `offset`, `sort`, `total`,
     `next_offset` and `at` on `/search`, the result cache, and qmd's
     ranking filtered by the structured terms. The UI still asks for
     everything in one page.
   - **4b: `pagedWindow.ts` and `GridCard` on it,** with header clicks
     sorting on the server and the header filter row becoming query
     terms. While grouped, the grid loads everything, until step 5.
5. **Server-side drag-to-group** in `GridCard`: the group list, a
   paged window per expanded group, and nesting.
6. **`/api/log` newest-first, and `RunLogPanel` on the same module.**
7. **Problems** (`TableGrid`), only if it grows large enough to need it.

## When the snapshot moves

On every `index_changed`, so every open grid refreshes together, as the
grid does today. The alternative was to hold a grid still until the
user asks for newer data, with a Refresh button or a "new rows —
refresh" banner. It stays available if live refreshes under someone
scrolling turn out to be distracting: the `at` in every response is
already what it would need.
