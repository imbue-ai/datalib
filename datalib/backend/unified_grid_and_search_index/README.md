# How a search becomes SQL, and which index answers it

A search is keys (`source:`, `from:`, `is:document`) and words. This
crate turns the keys into SQL over the grid index,
`unified_grid_and_search_index/db.sqlite`, a plain SQLite file. This page
is about that SQL: the statements a search runs, the indexes they read,
how SQLite chooses between them, and how we hold it to the right one.
What the columns mean is in [`grid_rows.md`](../../../docs/dev/grid_rows.md),
the query language in [`searching.md`](../../../docs/user/searching.md),
and what indexes cost and where they are going in
[`index_management.md`](../../../docs/dev/plans/index_management.md).

A search is fast while it reads index pages alone. `grid_rows` is wide,
and on a big library most of it is not in memory: fetching a row an index
named is a random read from disk, and a search that fetches a few hundred
thousand of them takes half a minute. **Nearly every slow search we have
found was one that read the table to test a second key.**

## The statements

Each is built from the same `WHERE`, `db::build_where`.

| what it is for | built by | reads |
|---|---|---|
| the listing: every row kept, in order | `dolt_repo::listing_sql` | indexes alone, when each key has one |
| what the words are ranked among | `db::matching_sql` | row ids from the indexes, then the narrow `rows` table |
| what a search by meaning cannot reach | `dolt_repo::documents_with_pages_sql` | row ids from the indexes, then `grid_rows_by_markdown_source` |
| a grouped grid's groups | `group::group_sql` | the kept rows, from the table |
| the values a key suggests | `group::values_sql` | the kept rows, from the table |
| one page's rows | `rows_by_uuids` | those rows |

`at:<node>` is read first: one statement asks which ways the node holds
rows, and the clause asks only for those (`at_node.rs`).

## The indexes

They are declared on `GridRow` (`#[portable_table(index = …)]`) and made
only in the grid index, not in the render stores that also hold a
`grid_rows`. There is one for the newest-first order, and one per key the
search bar filters on:

```
(key, touched_at_utc, is_document, uuid [, source_id])
```

The stamp puts a key's rows in the order the grid shows, `is_document` and
`uuid` let the listing be answered without the row, and `source_id` lets
the key beside `source:` be too. A key whose one value many rows share
(`kind`, `channel`, `subject`, `account`, `project`, `change`, and the
place and the conversation `at:` reads) has `source_id`; one whose value
names one thing (`id`, `notion_page`) has few rows to read. Each costs
about 0.1 GB on a 1.2M-row index, and `source_id` about 22 MB of it.

- `every_filter_key_is_served_by_an_index` fails when a key has none.
- `beside_a_key_a_source_is_tested_never_walked` holds every key, and
  `at:`, beside `source:`, in every statement above but the suggestions.

A filter with no index (`email:`, `org_name:`) and a negated key read
every row of whatever else the search names, or of the table.

## How SQLite chooses

No store runs `ANALYZE`, so SQLite guesses. Each of these is a test in
`planner_facts.rs`:

- **Two equalities are a tie.** It guesses that any `column = ?` with an
  index narrows as hard as any other, one channel and one half-million-row
  source alike. Which index a tie goes to can rest on the order they were
  made in. **So two files in the same shape can plan differently**: an
  upgrade makes new indexes last, where a new library makes them in
  order. Check a plan in both orders, and against a real file.
- **A `likelihood(…)` on one equality breaks the tie.** Ours say a flag
  keeps half of the table and a source, beside another key, a tenth.
- **An `OR` of two keys takes a third key's index.** `is:document` beside
  `place_uuid = ? OR conversation_uuid IN (…)` read one side of the `OR`
  from the documents' index: every document in the library. One set,
  `rowid IN (SELECT … UNION SELECT …)`, reads neither.
- **Two `IN` sets one index covers are tried in pairs**, each member of
  one with each of the other: 257,000 documents times 1,000 rows. A `+`
  before a column keeps it from its index, and leaves the set to be
  tested.
- **`ANALYZE` counts the rows with no value as one value.** A key most
  rows leave empty is rated wider than a source.
- **Our SQLite is built without `STAT4`**, so statistics are one average
  per column: it cannot tell a source of 585,000 rows from one of 2,000.
- **Statistics are rows of `sqlite_stat1`, which we may write**, and
  SQLite believes them whatever the table holds.

## So

- **A statement that can be answered from an index names it**
  (`INDEXED BY`), where one is always right.
- **Find the rows as a set of row ids first, and fetch last.** Every index
  ends in the row id, so `rowid IN (SELECT rowid FROM grid_rows WHERE …)`
  costs index pages, and what is read with it can be a narrow table or a
  covering index.
- **A key added to the search bar wants an index of the shape above**, and
  a place in both tests.
- **Test the plan, then time it on a clone of a real library**, with the
  app's own SQLite (`bazelisk build //third-party/doltlite:doltlite`,
  `file:<path>?doltlite_engine=sqlite`) and the statement as the code
  builds it. A hand-written near-copy, or the system's `sqlite3`, can plan
  otherwise. Split CPU from wall time: on a busy laptop the wall time is
  mostly whether the pages were in memory.

## What was measured

On a clone of one library (1.2M rows in 21 sources), on a busy 16 GB
laptop, with the indexes above.

**What to tell the planner.** The same plain statements, with neither
hints nor sets, each given up at 30 s. Likelihoods are one fraction per
column, on every predicate; the declared table is the same idea, written
into `sqlite_stat1` and so known to every statement.

| search | nothing | `ANALYZE` | likelihoods | declared |
|---|---|---|---|---|
| an empty channel in a 585k-row source, grouped | over 30 s | over 30 s | under 1 ms | under 1 ms |
| one account in that source, listed | 0.5 s | 21 s | 0.2 s | 0.2 s |
| `at:<page> is:document`, as an `OR`, listed | over 30 s | under 1 ms | under 1 ms | under 1 ms |
| that account and one kind, listed | over 30 s | 7 s | 5 s | 13 s |

`ANALYZE` took 133 s over `grid_rows`, rated `channel` at 64,760 rows a
value against a source's 58,592, and so started from the source; and the
account, at 410,143, lost its covering index. The last row has no index
that holds both keys, and no statistics help it. A declared table is
proposed, not built ([`index_management.md`](../../../docs/dev/plans/index_management.md)
§"Comments").

**The documents a search by meaning can reach.** The statement every
search with words waits for, as it is now, and as it was when it began at
those documents:

| search | began at the documents | now |
|---|---|---|
| `source:x from:y` | not finished at 40 s | 0.1 s |
| `source:x at:<page>`, a page with two ways | not finished at 40 s | 0.1 s |
| `is:document` | 7–8 s | 0.4 s |
| a 585k-row source, `is:document` | 8–58 s, 1.4 s of it CPU | 1–5 s, 0.6 s of it CPU |
| a 585k-row source alone | 0.15 s CPU | 0.5 s CPU |
| no filter | 0.18 s CPU | 0.27 s CPU |

The last two got slower: the walk of the whole index is what a large
answer costs. When the index is not in memory the walk is 2–3 s, once.
