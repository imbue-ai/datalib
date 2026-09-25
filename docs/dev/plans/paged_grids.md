# Paged grids: the search grid and the log card load a page, then more

*Proposal (2026-09-25); nothing here is built. Every number was measured
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

**The contract.** A paged endpoint takes the query, a direction and a
cursor, and returns rows plus cursors:

```
request:  q, limit, and at most one of  before=<cursor>  after=<cursor>
          (neither = the newest page)
response: rows (newest first), older: <cursor>|null, newer: <cursor>|null,
          at: <commit or seq the page was read at>
```

A cursor is the last row's sort key plus its tiebreak, encoded as an
opaque string: `(modified_at_utc, uuid)` by default for the grid (the
sort column's value when someone sorts by another), and `seq` for the
log. This is *keyset* paging (`WHERE key < ? ORDER BY key DESC LIMIT n`),
not `OFFSET`, so a page does not shift when rows land at the newest end
while someone is scrolling. `OFFSET 20000` measured 0.46 s, so offset is
affordable if a "jump to row N" is ever wanted. It is just not the
default.

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

**The default order is `modified_at` newest first, so every row gets
one.** Only 20,776 of the 74,023 rows have a `modified_at`. The
`GridRow` builder's `build()` sets `modified_at` to `created_at` when the
source gave none, so a record never touched after it was made counts as
modified when it was made. The UTC twin `modified_at_utc` follows,
because it is derived from `modified_at`. Doing it in the one builder
every render goes through means no provider changes, and the sort and
its index are one plain column rather than a `coalesce`. (An expression
index is used only when every query repeats the expression exactly.)
The cost is that the Modified column shows the creation time on those
rows. On the measured root, one row has neither stamp; it sorts last.

**A new `source_id` column** holds the group id. The `source_id:`
filter is `INSTR(qmd_path, 'gmail/') = 1` today, which no index can
serve, and it is the filter every Browse card uses.

**Indexes,** through `PortableTable`'s existing `index = "name:cols"`:

- `(modified_at_utc, uuid)` for the unfiltered grid;
- `(col, modified_at_utc, uuid)` for each key the search bar filters
  on: `source_id`, `provider`, `source_label`, `kind`, `channel`,
  `conversation_uuid`, `author`, `account`, `project`,
  `notion_page_uuid`, `diff_status`, `is_document`.

**Every column stays sortable,** server-side, in one of two ways:

- **When an index gives the order**, pages are keyset SQL straight off
  that index. That covers the default `modified_at` sort, and any
  filter plus the default sort. The cursor is the sort key plus `uuid`.
- **Any other sort goes through the result cache.** The first page
  runs the sort once, `SELECT uuid … WHERE <filter> ORDER BY <col>, uuid`
  (about 5 s unfiltered at this size, quick once an indexed filter has
  narrowed the set), and keeps the ordered uuid list. Every later page
  is a slice of that list plus a primary-key lookup of those 200 rows,
  so scrolling does not pay the sort again. The cursor is a position in
  the list plus the commit it was computed at. The status line says
  "sorting N rows…" while the first page waits, so the wait reads as
  expected rather than broken.

**The result cache** lives in the applet. It is keyed by
`(q, sort, grouping, commit)` and holds ordered uuid lists, group
lists and counts: whatever took a scan to compute. Everything that
would otherwise redo a scan per page goes through it: an unindexed
sort, qmd's ranked hits, the group list below, and the total. It
evicts least-recently-used entries under a byte budget. A uuid list
for all 74k rows is about 3 MB. An entry for an old commit is simply
never asked for again once the UI moves to the new one.

**Drag-to-group stays, and moves to the server.** Grouping by a column
is two kinds of request:

- **The group list:** `SELECT <col>, count(*) … WHERE <filter> GROUP BY
  <col>`. Every filterable column has a `(col, modified_at_utc, uuid)`
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
each seal by ~10 s. Step 2 times a real sync's seals before and after
the indexes. If seals turn out to hurt, moving the indexes to the read
branch is the fallback, and it is known to work: the merge kept every
index correct (`INDEXED BY` counts matched the table,
`integrity_check` ok). A test
asserts, per filter key, that `EXPLAIN QUERY PLAN` says
`USING INDEX`. It runs against a store of a few thousand synthetic rows,
so it is deterministic and fast, and it catches the next key someone
adds without an index.

This is a store shape change: a minor version bump, and a ladder rung
or a reset of `grid_index`. It is derived data, so the cost is a
re-index. The commit message says so.

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

**The search endpoint** gains `before`/`after` and returns cursors, with
a default `limit` of 200. `ORDER BY modified_at_utc DESC, uuid DESC`
replaces `created_at_utc ASC, is_document DESC, uuid`. A page stops
carrying `text`. The 240-character snippet is still computed on the
server; today a `Chat` row ships its whole `text` as the snippet, which
alone can be megabytes. The other `/search` callers (dactal's 2000,
perseus's 200000, `bridge.js`, the e2e specs) keep working unchanged:
no cursor means the newest page of `limit` rows.

**Counting** is a separate request (`/search/count`), sent after the
first page lands and cancelled with it. The status line reads "200
loaded" until the count arrives, then "200 of 46,566". An indexed
filter counts in about 0.1 s. A free-text `LIKE` count scans, so it is
skipped and the line says "200+".

**Free text via qmd** is already capped at about 1000 ranked hits.
Those are paged by rank out of the result cache, rather than asking
qmd again per page. Separately, every
free-text search loads all of `grid_rows` through `grid_row_refs()` to
map hits to rows. That becomes a lookup by `qmd_path` over an index.

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

Its cost barely depends on how many rows the grid holds. Every call
runs `summary()`, an aggregate over the whole `content_vectors` table,
and each chunk of 400 hashes repeats the same `GROUP BY`. It also opens
a new pool per request and hashes every `.md` file it is sent, and past
2000 documents it truncates and toasts on every call. The fix: send the
loaded rows' uuids, cache `summary()` against the qmd index's mtime,
and compute the per-hash aggregate once per call. It is its own PR,
and it can land first.

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
2. **`grid_rows` columns and indexes, and the applet's snapshot
   connection** in the search path. Default order newest-first. The UI
   is unchanged, so it still asks for everything, but the query stops
   scanning to sort. Includes the index-per-filter-key test, and times
   a real sync's `grid_index` seals before and after the indexes.
3. **`qmd_state` fixes** (independent of the others).
4. **The page contract, the result cache and `pagedWindow.ts`, and
   `GridCard` on them,** with server-side sorting.
5. **Server-side drag-to-group** in `GridCard`: the group list, a
   paged window per expanded group, and nesting.
5. **`/api/log` newest-first, and `RunLogPanel` on the same module.**
6. **Problems** (`TableGrid`), only if it grows large enough to need it.

## When the snapshot moves

On every `index_changed`, so every open grid refreshes together, as the
grid does today. The alternative was to hold a grid still until the
user asks for newer data, with a Refresh button or a "new rows —
refresh" banner. It stays available if live refreshes under someone
scrolling turn out to be distracting: the `at` in every response is
already what it would need.
