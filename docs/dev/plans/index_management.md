# Index management: what a library's indices cost, and who pays

*Proposal (2026-11-24); nothing here is built. Every number was
measured that day on an APFS clone of one real library (1.22M grid
rows in 21 sources), on a 16 GB laptop, with the `sqlite3` shell
(SQLite 3.51) against the clone's grid index and inference cache, and
from the search eval's runs on that clone. Timings marked "warm" ran
with the file's pages already in memory, "cold" without.*

Read [`documents_to_blocks.md`](documents_to_blocks.md) §"How search
works" first: this plan is about the files and indices underneath it.

## The problem

A library's sources differ in size by five orders of magnitude, and
every search pays for the biggest of them.

| source (described, not named) | grid rows | keyword pieces | embedded chunks |
|---|---|---|---|
| a whole Slack workspace | 585,197 | 319,814 | 321,640 |
| Claude Code sessions | 392,549 | 391,006 | 506,808 |
| a smaller Slack export | 82,362 | 40,689 | 40,853 |
| a whole Notion workspace | 61,796 | 60,966 | 72,701 |
| Gmail | 49,492 | 28,396 | 38,402 |
| the other 16 sources | 50,822 | 80,288 | 51,765 |

The first two hold 80% of the grid's rows and 80% of the chunks
embedded for meaning.

Everything search reads is in two files under
`unified_grid_and_search_index/`, one for all sources:

- **The grid index**, `unified_grid_and_search_index/db.sqlite`:
  5.52 GB. `grid_rows` is 1.27 GB and its 18 indexes another 1.8 GB,
  more than the table. The keyword index is small beside them: the
  full-text table's data is 0.42 GB, and with `search_sections` and
  `search_documents` 0.53 GB in all. The rest is `markdowns` (0.28),
  `data_nodes` (0.23), `source_contacts` (0.23), the search terms'
  tables (`vals_tri`, `terms`, `vals`: 0.30) and the indexes on them.
- **The inference cache**, `search_index/inference_cache.sqlite`: 5 GB
  on disk, holding 932,351 distinct vectors of 768 dimensions in 32-bit
  floats (2.9 GB of vector bytes) for 1.03M chunks, in 4 KB pages: the
  rewrite to 16 KB pages was put off while the app held it open (#1969)
  and then removed (#1970), before it had run on this library.
  The search applet holds a vector for every chunk in memory, in half
  precision, about 1.5 GB.

On a 16 GB laptop with the app and its usual company running, these do
not stay in memory. The OS held about 2.6 GB of files in memory during
the eval, and a search's SQL spent most of its time in `pread`, reading
pages back from the SSD (a 30 s `sample` of the applet). The same
statement, limiting a common word's matches to one small source, took
0.51 s cold and 0.07 s warm.

Nobody decides what an index is worth. The grid has one index per
filter key (`every_filter_key_is_served_by_an_index` holds each key to
one), most shaped `(key, touched_at_utc, is_document, uuid)` so a
filtered listing pages by time without sorting. Each costs about 0.1 GB at this
size, whether anyone filters by that key or not.

## What the keyword index costs, measured

Finding a word's matches is cheap; scoring them is not.

| statement, warm | rows | time |
|---|---|---|
| `MATCH '"the"'`, counted | 418,251 of 921,159 pieces | 0.01 s |
| the same, joined to `search_sections` | 418,251 | 0.05 s |
| the same, joined on to `search_documents` | 418,251 | 0.07 s |
| the same, limited to one small source | 15 | 0.07 s |
| the same, scored with `bm25()`, best 1,000 kept | 1,000 | 0.58 s |
| `"the" AND "meeting" AND "notes"`, joined | 1,741 | 0.006 s |

The vocabulary holds 622,098 terms; 102 of them are in more than a
tenth of all pieces, the commonest in 59%. A statement that scores one
common word alone scores half the library, and that is what the page
pass does for each word of a query
([`documents_to_blocks.md`](documents_to_blocks.md) rule 3). A
statement over all the words together scores only the pieces holding
all of them. So the keyword index's layout is not what makes search
slow today: the statements are, and the memory the grid takes.

## Goals

1. **A search's cost follows its scope and its answer.** Searching one
   small source does not pay for the biggest one; adding a big source
   does not slow searches that leave it out.
2. **A library knows what its indices cost.** How much memory a fast
   search needs, and which source and which index take it, is a number
   the app can show, not something found with `dbstat`.
3. **A source's indices can change alone.** Rebuilding, leaving out of
   search, or removing one source touches its own indices, not every
   source's.
4. **Scores stay comparable across sources.** A search of everything
   ranks one source's hit against another's on one scale.

## The choices

**Keep one shared index, and fix what costs** (what this plan starts
with). The statements change as `documents_to_blocks.md` rule 3 says:
a source and the keys filter in the same statement as the match, so
`bm25()` runs only on what is kept, and the page pass never scores a
word alone. That is where the measured time is. It does nothing for
goals 2 and 3.

**One full-text table per source, in the one file.** A scoped search
asks only its sources' tables; a search of everything asks each table
and merges. It gives goal 1 for keywords. It breaks goal 4: `bm25()`
weighs a word by how rare it is in its own table, so a word rare in
Gmail and common in Slack scores differently in each, and the merged
ranking is not one scale. Keeping one scale means scoring ourselves,
with each word's rarity counted across every table (FTS5's `fts5vocab`
gives each table's counts; the per-match term counts need an FTS5
auxiliary function, which is C, in the shim beside the embedder's).

**One file per source**, attached for the search that needs it. Goal 3
comes with it: a source's index is rebuilt, left out or deleted as a
file, and the OS keeps in memory the files searches read. The grid's
own rows could split the same way, which is where most of the bytes
are. The costs: everything that reads the grid as one table (the grid
is one denormalized table filled from every source's render store, and
read with one `SELECT`) becomes a `UNION ALL` over attached files; SQLite attaches at most 10
databases unless built with a higher limit (up to 125); and goal 4's
scale problem is the same as per-table.

**Packing source, document and block into the full-text row number**
was considered and not taken
([`documents_to_blocks.md`](documents_to_blocks.md) rule 3).

The order below starts with the first, measures, and leaves the split
to be decided on numbers.

## Order of work

1. **The statements** (`documents_to_blocks.md` rule 3): filter before
   scoring, and a page pass that scores the words together. Checked on
   the search eval's timing set and its SQL audit (#1942); the target is
   a sentence-length query under a second warm.
2. **What each source costs, reported.** The index step already reports
   what search holds of each source; it adds the bytes: grid rows and
   their share of the grid's indexes, keyword pieces, chunk vectors.
   Manage shows it beside the source, and the library's total beside
   the machine's memory.
3. **Which grid indexes earn their space.** The SQL census
   ([`sql_census.md`](sql_census.md)) proposes recording the plan of
   every statement the tests run; run over the eval too, it names the
   index each statement uses. An index no statement uses goes, unless a key a person types
   needs it, in which case the key's statement says so in a test.
4. **The inference cache shrinks.** Prepare rewrites its pages to 16 KB
   at the next launch. Whether vectors are kept in half precision on
   disk too, as they are in memory, is measured against the eval.
5. **The split, decided on numbers.** With 1 to 4 done, measure a
   search scoped to a small source against a search of everything, cold
   and warm, on the largest library to hand. If a scoped search still
   pays for the big sources, split the keyword index per source, with
   scoring of our own for one scale; if memory is still the limit, split
   the grid per source too.

## Open questions

- **What a fast search may assume about memory.** Should a library
  name the memory its search needs, warn when the machine has less, or
  shed indices (a key nobody filters by) to fit?
- **The grid's index shape.** Each filter key's index carries
  `touched_at_utc, is_document, uuid` for paging. Would one index per
  key on the key alone, with the time order read from `grid_rows`, page
  fast enough, at a fraction of the bytes?
- **Scoring of our own.** If the keyword index splits, is BM25 with
  rarity counted across all tables the right scale, and does the eval
  notice the difference from today's per-table weights?
- **Where meaning's vectors live.** The applet holds every vector in
  memory, 1.5 GB here. A scoped search could score only its sources'
  vectors, but they are all held regardless. Hold per source, loaded
  on first use?

## Comments

**2026-11-24, from building the Library's place lists (#2002).** Measured
in SQL on a clone of the library above, with `grid_rows_by_parent` and
`nodes_places` from #1998 and the index #2002 adds, `grid_rows_by_place`
(140 MB, the usual `(key, touched_at_utc, is_document, uuid)` shape).

- **The grid's index shape: an answer for one key.** Listing a channel's
  34,027 documents newest first takes 29 ms cold and 11 ms warm when the
  index holds the order, so no row of the table is read. The same rows
  found through the index and then read from `grid_rows` by `rowid` take
  1.9 s, 0.25 s of it CPU and the rest reading pages, each time it was
  asked. An index on the key alone would be the second kind. So for a key
  that lists many rows the stamp, the flag and the uuid are most of what
  the index is for. A key whose values match few rows could go without.
- **An index that serves a key can lose to one that does not.** The plan
  test holds each key alone to its index. `source:slack channel:x`, with
  no row in that channel, takes 1.5 s: with no statistics SQLite counts
  any equality as narrowing as hard, takes `grid_rows_by_source_id` on the
  tie, and reads the source's 82k rows from the table, where
  `grid_rows_by_channel` answers in under 1 ms. An `OR` of two keys does
  it too, since SQLite's `OR` of two indexes always reads the table. So
  step 3 should record the rows a statement walked, not only which index
  it named, and ask of pairs of keys. A hint on `source_id` alone moves
  `source:x is:document` from 7 ms to 181 ms, so it is not the fix.
- **An index to ask step 3 about.** `grid_rows_by_parent` (138 MB) serves
  only `GET /children/{id}` without `places=true`. The Library's pane was
  what read that; since #2002 the app's own pages list places alone, off
  `nodes_places` (8 MB). What is left is the public route. (2026-11-25:
  both are gone. `GET /places/{id}` lists the places, and opening the grid
  index as its owner drops an index this build does not make.)
- **Per-source files would suit a place.** A place is in one source, so
  `at:<node>` names the one file to read, with no `UNION ALL`. Only a
  search that names neither asks every file.
- **`under:` may need no table of its own.** Before it was put off
  (`documents_to_blocks.md` step 8), walking `nodes_places` down from a
  node and reading `grid_rows_by_place` for every place found took 5 ms
  for a mail account, 36 ms for 32k Notion pages and 70 ms for a Slack
  workspace of 135,496 threads. The ancestors table would be a few
  million rows. What the walk does not give is the result in time order,
  which a page of it then sorts.

**2026-11-24, from fixing `source:` beside another key.** Measured in SQL
on a clone of the library above. `source:x channel:y` was one case of
two keys sharing no index: whichever one SQLite starts from, it reads
each of that key's rows from the table to test the other. Times are the
first run, then the best of three; the second column was asked once.

| search, rows it finds | source first or key first, as it was | row-id sets | `source_id` in the key's index |
|---|---|---|---|
| a channel with no row, in a 585k-row source: 0 | 43 s | 1.6 s / 17 ms | 1 ms / under 1 ms |
| one kind in that source: 319,813 | 22 s | 0.76 s / 0.32 s | 0.36 s / 55 ms |
| that kind in an 83k-row source: 40,834 | 23 s | 0.19 s / 0.13 s | 17 ms / 17 ms |
| one account in the 585k-row source: 584,885 | 33 s | 1.2 s / 0.70 s | 0.74 s / 0.13 s |
| that account in a 2k-row source: 2,019 | 25 s | not measured | 42 ms / 42 ms |

- **What was done: the third.** Six keys whose one value many rows share
  (`kind`, `channel`, `subject`, `account`, `project`, `change`) end in
  `source_id`: about 22 MB each, 130 MB in all, 2.4% of the file. So do
  the conversation's index and #2002's `grid_rows_by_place`, which
  `at:<node>` reads. `id` and `notion_page` name one thing and stayed as
  they were.
- **A hint on the source alone is not a fix.** It starts from the key,
  which is worse wherever the key is wider than the source: one account
  covers 737k rows in seven sources. The hint is still there, for the
  statements no index covers (a grouping reads the rows it counts), but
  only beside a key, so `source:x is:document` keeps its plan.
- **Row-id sets cost no bytes, and answer any pair.** The source's index
  is walked and each entry tested against
  `rowid IN (SELECT rowid FROM grid_rows WHERE channel = ?)`; the table
  is never read. It costs what the bigger key's index holds, so it does
  not meet goal 1 where one key is narrow. Two keys with no source among
  them (`account:a kind:k`) still read the table, and this would fix
  them.
- **A filter no index serves still reads every row of its source**:
  `source:x -channel:y`, `source:x email:z`. A negated key can be a
  row-id set (`rowid NOT IN (…)`) too.
- **The grid's index shape** (the open question above): an index on the
  key alone would give back every number in the second column. Not reading the
  table is what the covering columns buy.
- **Two to ask step 3 about.** `grid_rows_by_markdown (markdown_uuid)`,
  59 MB, is a prefix of `grid_rows_by_markdown_source (markdown_uuid,
  source_id)`, 83 MB, and both are in the grid index.
  `grid_rows_by_diff_status` is 89 MB here for a column that is null in
  every row; `grid_rows_by_channel` as much for 33k rows that have one. A
  partial index (`WHERE channel IS NOT NULL`) still serves `channel = ?`,
  but not the "none" group of a grid grouped by channel, which reads the
  nulls through it.
- **An `OR` of two ways to be at a node** (`at:<node>`, #2002) reads one of
  them from whatever else the search names: `at:<page> is:document`, for a
  page that is a document and holds 7,048 rows, read every document in the
  library from the table, 24 s. Written as one set of row ids it takes 19 ms,
  then 2 ms. A label with 14,506 threads filed under it, beside its source:
  1.3 s before, 58 ms with `source_id` in the conversation's index, 0.3 s
  with the hint alone (first runs, the later two on a warmer cache).
- **Two sets of row ids are a product.** The count of what a search by
  meaning cannot reach lists the documents it can, 257,407 of them, and
  SQLite began at those: with `source:x from:y`, it tried each with each of
  the person's 1,028 rows, and was stopped at 60 s. Kept from starting
  there (`+markdown_uuid`), 0.4 s; and with `is:document` alone, 10.9 s
  before and 0.7 s after, both asked twice.
