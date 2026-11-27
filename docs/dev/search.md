# How search works

For someone changing search: what a search does from the typed line to
the rows shown, what it reads, how its SQL is kept fast, and how to
measure it. The query language is in
[`searching.md`](../user/searching.md); the columns searched are in
[`grid_rows.md`](grid_rows.md). Where search is going is in two plans:
[`documents_to_blocks.md`](plans/documents_to_blocks.md) §"How search
works" and [`index_management.md`](plans/index_management.md).

## Where things are

| what | where |
|---|---|
| the search box's grammar, with no table in it | `query/` (`datalib_query`) |
| the keys a grid takes, the SQL they become, the repo that runs it | `unified_grid_and_search_index/` |
| the keyword index, the chunks and the vectors | `search_index/` |
| text to vectors | `embedder/` ([README](../../datalib/backend/embedder/README.md)) |
| a request, from its query to its answer | `applets/src/unified_grid_and_search_index/` |
| the writers | `etl/render/src/grid_index.rs` for the grid index, `search_index/src/store.rs` for the words in it, `datalib_step` for each source's `embed` |

All are under `datalib/backend/`. The applet is the one server of a
search: the app, `datalib-applet search` and an agent's HTTP request get
the same answer from it.

## What a search reads

Two plain SQLite files under `unified_grid_and_search_index/`, for every
source together. Neither is doltlite.

**The grid index**, `unified_grid_and_search_index/db.sqlite`, written by
the `unified_grid_and_search_index` step alone:

- `grid_rows`: one wide row per thing the grid can show, with its indexes.
- `rows`: the same rows, narrow: each one's row id, uuid and document.
- `terms`, `vals`, `names` and their full-text tables: who and what each
  row names, for `from:`, `label:` and an id or address typed as it is.
- `search_fts`, `search_sections`, `search_documents`: the keyword index.
  One row per *piece* of a document, a message or a stretch of a page; the
  postings and where each piece falls, never the text.
- `markdowns`, `nodes`, `filings`, `source_contacts` and the rest of what
  the grid shows.

**The inference cache**, `search_index/inference_cache.sqlite`, written by
each source's `embed` step, one at a time: where each chunk of a document
falls, and each chunk's vector under the hash of the prompt the model was
shown. The applet holds a vector for every chunk in memory, in half
precision.

A reader requires the shape this build writes, checked inside each read
([`store_meta/README.md`](../../datalib/backend/store_meta/README.md)),
and reads inside one transaction, so one answer is of one state of the
index.

## What a search does

`answer_search` in the applet. A query is parsed into **terms** (the keys,
and the free text that filters: a quoted phrase, `+word`, `-word`, an id
or an address) and **free text** (the words to rank by). `with_people`
then turns `me` and `contact:` into handles.

**With no words**, the terms become one SQL `WHERE`, and the listing is the
uuids it keeps, newest first or in the sort asked for. A page is then read
by uuid.

**With words:**

1. **The terms become the rows they keep** (`keyed`), and per document
   either all of it or certain messages (`kept_of`). Nothing is ranked
   that a term would drop, so a cut never empties a filtered search.
2. **Words rank documents** (`word_hits`): BM25 over the pieces holding
   the words, each document at its best piece. A page, whose pieces are
   parts of one thing, may hold the words between its pieces; a
   conversation's message must hold them itself.
3. **Meaning ranks documents** (`meaning_ranking`), unless `semantic:off`:
   the query is embedded and scored by cosine against every chunk vector
   of the kept documents, down to the cutoff.
4. **The two are fused** by reciprocal rank (`fuse`), one entry a document,
   and cut at `RANK_DEPTH`.
5. **Each document shown opens on one of its rows** (`land`): the
   message its hit named where the terms keep it, and it is quoted from
   that row's text.

A part that fails costs that part and says so. The list and what the terms
keep are cached under the query and the index's generation, so the next
page slices what the first one ranked.

## The statements, and what each reads

Each takes its `WHERE` from `db::build_where`.

| what it is for | built by | reads |
|---|---|---|
| the listing | `dolt_repo::listing_sql` | indexes alone, when each key has one |
| the rows the terms keep, for ranking (step 1) | `db::matching_sql` | row ids from the indexes, then `rows` |
| a grouped grid's groups | `group::group_sql` | the kept rows, from the table |
| the values a key suggests | `group::values_sql` | the kept rows, from the table |
| one page's rows | `rows_by_uuids` | those rows |

`at:<node>` is looked at first: one statement asks which ways the node
holds rows (`at_node::FACTS_SQL`), and the clause asks for those alone.

**A search is fast while it reads index pages alone.** `grid_rows` is wide,
and on a big library most of it is not in memory: fetching a row an index
named can be a read from disk, and a search that fetches a few hundred
thousand takes half a minute. Nearly every slow search we have found read
the table to test a second key.

### The indexes

Declared on `GridRow` (`#[portable_table(index = …)]`), and made only in
the grid index, not in the render stores that also hold a `grid_rows`.
One is for the newest-first order, and one for each key the search bar
filters on:

```
(key, touched_at_utc, is_document, uuid [, source_id])
```

The stamp puts a key's rows in the order the grid shows, `is_document` and
`uuid` let the listing be answered without the row, and `source_id` lets
the key beside `source:` be too. A key whose one value many rows share
(`kind`, `channel`, `subject`, `account`, `project`, `change`, and the
place and the conversation `at:` reads) ends in `source_id`; one whose
value names one thing (`id`, `notion_page`) has few rows to read. Each is
about 0.1 GB on a 1.2M-row index, `source_id` about 22 MB of it.

A filter with no index (`email:`, `org_name:`) and a negated key read
every row of whatever else the search names, or of the table.

### What the planner is told

SQLite chooses an index by how many rows it thinks a value has. With
nothing to go on it takes every key to narrow alike, and reads a whole
source to find none of it in a channel. We tell it, once, in the table it
reads its statistics from: `sqlite_stat1`, written by
`grid_index/planner_ranks.rs`, the same in every library, and part of the
index's shape.

| rank | rows per value, of a pretended million | of |
|---|---|---|
| flag | 100,000 | `is:document` |
| source | 10,000 | `source` |
| many | 1,000 | kind, account, project, change |
| some | 100 | channel, place, author |
| few | 10 | conversation, subject, page, location |
| one | 1 | id |

**These are ranks, not counts.** The planner compares them and nothing
else, so they need only be in order and far apart, and nothing is measured
in a library. True numbers are worse: a source as a quarter and a kind as
a twentieth are near where SQLite would rather read an index whole than
search it, and plans then turn on small differences. Of 100 tables with
every number wrong by up to 1.5 times, none gave a wrong plan for any of
309 statements; at 3 times, 4 did.

The order can be wrong for one library, a small source beside a large
account. Beside `source:` a wrong order costs little, since the key's
index holds the source. Between two other keys (`account:a kind:k`) no index holds
both, and no figure is right for every value: it reads the rows of one.

Two things statistics cannot say:

- **A set of row ids** (a person's rows, a quoted phrase's) is taken to be
  25 rows, whatever it is, and read from the table. Beside a key an index
  starts with it is tested instead, with a `+` (`db::set_of`); alone, a
  search starts from it.
- **A range** (`near:` is seven, ORed) has no figure, so it carries a
  `likelihood`.

Dropping an index deletes its row of `sqlite_stat1`, so the code that
makes the indexes again (`init_schema` and `create_search_indexes` in
`etl/render/src/grid_index.rs`) writes the ranks first and makes the
indexes after: making an index changes the schema, and that is what
makes a connection already open read `sqlite_stat1` again.

### What SQLite's planner does

Each of these is a test in
`unified_grid_and_search_index_tests/planner_facts.rs`, so a new SQLite
that changes one says which:

- **Two equalities with no statistics are a tie**, and which index it goes
  to can rest on the order they were made in. So two files in one shape
  can plan differently: an upgrade makes new indexes last.
- **A `likelihood(…)` on one equality breaks the tie.**
- **An `OR` of two keys, with no statistics, takes a third key's index**:
  `is:document` beside two ways to be at a node read every document.
- **Two `IN` sets one index covers are tried in pairs.** A `+` before a
  column keeps it from its index, and leaves its set to be tested.
- **`ANALYZE` counts the rows with no value as one value**, so a key most
  rows leave empty is rated wider than a source.
- **Our SQLite is built without `STAT4`**, so statistics are one average
  per column: it cannot tell a source of 585,000 rows from one of 2,000.
- **Declared statistics are believed whatever the table holds**, so an
  empty file plans as a full one, and a test can check a plan on an
  empty file.

### Why not `ANALYZE`

It measures what a store holds when it runs, so two libraries plan one
statement two ways ([`etl/README.md`](../../datalib/backend/etl/README.md)
§"Indexes"), and it was measured to be wrong here. On a library of 1.2M
rows in 21 sources it took 133 s over `grid_rows`, rated `channel` at
64,760 rows a value against a source's 58,592, and so started at the
source. The same plain statements, each given up at 30 s:

| search | nothing said | `ANALYZE` | ranks |
|---|---|---|---|
| an empty channel in a 585k-row source, grouped | over 30 s | over 30 s | under 1 ms |
| one account in that source, listed | 0.5 s | 21 s | 0.3 s |
| `at:<page> is:document`, a page with two ways, listed | over 30 s | under 1 ms | 10 ms |
| that account and one kind, listed | over 30 s | 7 s | 5 s |

### Adding a key or an index

1. Give the key an index of the shape above, or put it in `SCANS` in the
   test and say why people rarely narrow by it.
2. Give the index a rank; `every_index_of_the_grids_rows_has_a_rank_and_no_other`
   fails without one.
3. `every_filter_key_is_served_by_an_index` checks each key's plan uses
   its index, and `beside_a_key_a_source_is_tested_never_walked` checks it beside
   `source:`, `is:document`, `after:`, a person and an unindexed key, in
   every statement above but the suggestions, with the indexes made in
   either order.
4. Where one index is always right, name it (`INDEXED BY`).
5. Find rows as a set of row ids first, and fetch last: every index ends in
   the row id, so `rowid IN (SELECT rowid FROM grid_rows WHERE …)` costs
   index pages.

## Measuring

**The plan first**, in a test, over an index the step's own code made.

**Then the time, on a clone of a real library** (`cp -c`; never the live
one), with the app's own SQLite and the statement as the code builds it:

```sh
bazelisk build //third-party/doltlite:doltlite
bazel-bin/third-party/doltlite/doltlite -readonly 'file:<clone>?doltlite_engine=sqlite'
```

A near-copy of the statement, or the system's `sqlite3`, can plan
otherwise: both have misled us. Take a cold run (a fresh clone is cold)
and a warm one, and CPU apart from wall time.

**Then the whole search**, with `scripts/search_eval.py`
([`testing.md`](testing.md) §"Search quality on a real library"): ranks
and times over a set of queries, kept beside the library it was made from.
A slow search worth fixing is worth a case there: a target in the biggest
source, beside a key that keeps much of it, and for a search with no
words, one of its `orderings`.

On that library, with the ranks, best of two runs, in seconds:

| search | listed | grouped by author |
|---|---|---|
| a 585k-row source | 0.11 | 1.6 |
| it, `is:document` | 0.05 | 3.8 |
| it, an empty channel | 0.00 | 0.00 |
| it, one kind (320k rows) | 0.07 | 4.2 |
| it, one account (585k rows) | 0.13 | 4.7 |
| it, one person (542 rows) | 0.03 | 0.03 |
| that account in a 2k-row source | 0.04 | 0.05 |
| `at:<page> is:document` | 0.01 | 0.01 |
| a label with 14.5k threads, beside its source | 0.03 | 0.08 |
| one account and one kind, no source | 4.5 | 8.4 |

Grouping reads each kept row for its author; the last row is the one no
index serves.
