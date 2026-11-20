# The SQL census: every statement the tests run, and whether it scales

**Status: proposed 2026-11-20, nothing built; tracked in #1942.** The pieces it builds on
are in the tree or in review: `datalib_sql_work`'s statement counter and
the fixture sync's scan census (#1933, #1936), and `record_under` with
the grid index's small-beside-large test (#1937).

## 1. The problem

Every recent slow-index bug had one shape: a statement that runs once
per source, or once per document, did work in proportion to the whole
store instead of to what it was asked about. On a test store of a few
rows such a statement is instant, so no test noticed, and each was
found on a real library after it shipped (#1679, #1926, the render
store read #1937 fixes).

We check for this in three narrow ways today:

- **Plan tests** (`EXPLAIN QUERY PLAN` on one statement) check the
  statements someone thought to check. A plan with no `SCAN` can still
  read most of a table through an index on a column with two values,
  as #1926's did.
- **The fixture sync's census** (`//tests/fixtures:sql_scans_test`)
  lists every statement that walked a whole table during the TNG sync.
  It asks one question, a full scan, and only of the sync.
- **The small-beside-large test** (`grid_index::work_tests`) compares
  each statement's work with and without a large source beside it. It
  covers one pass of one step.

What we lack is the whole picture: **which statements the tests run,
which they never run, and which would slow down on a large library.**
The tree holds about 2,500 `sqlx::query*` call sites in 322 files,
about 370 of them built at runtime (`AssertSqlSafe`). Nobody knows how
many of them a test reaches.

## 2. What we want

1. **Every SQL call site no test runs, listed with a reason**, and a
   check that refuses a new one without a reason. A reason is a debt,
   not an excuse: "only a live account reaches it" and "only the
   Playwright suite reaches it" are tests to write (§4).
2. **A record of every statement the tests do run**: the kind of store
   it ran on, the tests that ran it, its work (SQLite's VM steps, rows
   answered, full-scan steps) and its query plan.
3. **A verdict on whether each executed statement scales**: does its
   work grow with rows that are not its own?

None of this runs in the everyday gate. It is a mode a person or a
nightly CI job turns on.

## 3. The plan

### 3.1 A census mode that reaches every process

Today the counter is installed by four binaries' `main` (`datalib-step`,
`datalib-dag`, `datalib-http`, `datalib-applet`) and by
`datalib_sql_work`'s own tests. A test binary that opens a store in its
own process is not counted. Installing the counter in
`datalib_sqlite_open` would not be enough either: about 35 non-test
files open a store some other way (`SqliteConnectOptions::from_str`,
`doltlite_raw`, `sql_guard`'s own open).

**Recommended: make the hook part of SQLite's own start-up.** Every
binary links our one SQLite, `//third-party/doltlite:sqlite3`. SQLite's
`SQLITE_EXTRA_INIT` compile option names a function that
`sqlite3_initialize` calls once per process; ours would register the
counter. Every process that touches SQLite is then counted with no call
to remember, test binaries and the doltlite shell included. The
function would live in a small C file beside the amalgamation that
forwards to `datalib_sql_work` when that is linked and does nothing
when it is not (a weak symbol).

**Census mode is one switch.** `tools/run_sql_census.sh`, shaped like
`tools/run_coverage.sh`, runs `bazelisk test //...` and the Playwright
suite with:

- `DATALIB_SQL_CENSUS` set, and passed to every child a test starts
  through `DATALIB_CHILD_ENV`, as coverage passes `LLVM_PROFILE_FILE`
  ([`coverage.md`](../coverage.md) §6). A step not handed the variable
  writes nothing, and the report would look complete without it.
- each test's census written under `TEST_UNDECLARED_OUTPUTS_DIR` and
  named by `TEST_TARGET`, so the merge knows which test ran what;
- `--nocache_test_results`, because a cached test writes nothing.

**What the census records, per process:** each distinct statement, its
runs, its costliest run's work, and its `EXPLAIN QUERY PLAN` once. A
statement is the text SQLite reports with its whitespace collapsed, and
runs of `(?, ?, ?), (?, ?, ?)` or `IN (?, ?, ?)` collapsed to one
group, so a statement built per chunk size counts once. Its store is
the file's path under the test's folder, with each source's own folder
named alike, as #1937's test does. Statements SQLite runs for itself
(FTS5's shadow tables, `sqlite_master`, the `pragma_*` functions) are
recorded and marked as SQLite's.

**Values stay out of the report.** For the replay in §3.4 the census
also keeps one run's bound values of each distinct statement. Test data
is made up, but the mode must never be turned on against a real
library: it refuses a store outside a test's temporary folder.

### 3.2 The call sites no test runs

Coverage already says which lines a test executed
([`coverage.md`](../coverage.md)). **A call site is covered when its
line is.** That holds for SQL built at runtime too: it is the call site
that is covered, not the text.

- A lister (Rust, over `syn`) writes every `sqlx::query*` call site in
  the tree, with its file, line, crate and, for a literal or a `const`,
  its SQL.
- The census run is also a coverage run; the merge reads its lcov and
  marks each call site run or not.
- Literal SQL is matched to the census by its text, so each executed
  statement names its call site. Runtime SQL is matched by its shape,
  and where it cannot be the report says so.

The uncovered call sites go in `tests/sql_uncovered.toml`, each with
one of a few reasons:

| reason | it runs only… | what closes it |
|---|---|---|
| `error_path` | when a statement or a fetch fails | a test that makes it fail |
| `old_shape` | on a store shape no fixture holds | a shape in `datalib_step/raw_shapes/` |
| `live_only` | against a live account | an HTTP playback fixture (`datalib_etl_web`) |
| `e2e_only` | under the Playwright suite | a Rust test that calls the route |
| `tool` | in a binary or tool with no tests | a test, or deleting the tool |
| `dead` | never: nothing calls it | deleting it |

The check refuses a call site that is neither run nor listed, and an
entry whose call site now runs, so the list only shrinks. It runs where
the census runs, not in the gate.

### 3.3 A rule over every executed statement's plan

We never run `ANALYZE`, so SQLite plans a statement the same way
whatever its tables hold: **the plan a test store gets is the plan a
large library gets.** So each recorded plan can be judged once:

- A `SCAN` of a table is allowed only by an entry with a reason. This
  takes over `tests/fixtures/sql_scans_allowed.toml`, one list for the
  whole suite.
- A `SEARCH` through an index whose leading column has few values is
  flagged. Those columns are declared once (`grid_rows.is_document`,
  `provider`, `kind`, …); an undeclared column is taken to have many.
  This is #1926's shape.
- A table searched under a `CORRELATED` subquery, an `AUTOMATIC` index,
  and a temp B-tree over a table read whole are flagged.

### 3.4 Replaying each statement on a store made large

A plan rule cannot see everything: a plan can be right and the
statement still answer with every row of a source when it was asked for
one document. So, in census mode, **each distinct statement runs again
on a copy of its store made large with rows that are not its own**, and
its work is compared.

- **When:** as the last connection to a store closes (SQLite tells the
  trace hook), the census copies the file, in its final committed
  state.
- **Filler:** the copy is filled with rows that belong to someone else.
  Each table names its owner columns (`source_id`, `markdown_uuid`, an
  account): a `#[col(owner)]` mark on the `PortableTable` derive, and a
  short list for the tables with no derive (the search tables, FTS).
  Filler rows are copies of the store's own rows with only those columns
  changed, about fifty times as many. Every other column keeps its real
  values, so a statement that reads through `is_document` still walks
  the filler, as #1926's did.
- **Replay:** each recorded statement runs on the copy twice, before and
  after the filler, with its recorded values; a write runs inside a
  savepoint that is rolled back. Both runs see the same state, so the
  comparison is fair however the store changed after the statement
  first ran.
- **Verdict:** a statement whose work grows by more than a small factor
  is reported, unless it is listed as reading the whole store by design
  (as the grid index's `INDEXED_DOCUMENTS` is in #1937), with a reason.
- **Not replayable, and reported as such:** a statement that reads a
  temporary table, one that names `HEAD` on a doltlite store the filler
  moved, a schema statement.

#1937's test stays: it runs in the gate, in two seconds, on the pass
most likely to regress.

## 4. Closing the gaps it finds

The uncovered list is a to-do list. Two kinds are expected to be most of
it and deserve their own work:

- **Network paths only a live account reaches.** Each network source
  gets a playback fixture covering its listing, its fetch and its error
  replies.
- **Server routes only the Playwright suite reaches.** A Rust test of
  `datalib-http` or the applet calls the route over a TNG root.

## 5. Steps

1. **Census mode** (§3.1): the init hook, the script, the merged report
   of every executed statement. Done when one run lists statements from
   a test binary, a step, the server and an applet.
2. **Uncovered call sites** (§3.2): the lister, the join with coverage,
   the first `sql_uncovered.toml` with a reason per entry, the check.
   Done when the nightly job fails on a new call site with no reason.
3. **Plan rule** (§3.3), taking over the fixture sync's scan list.
4. **Replay** (§3.4): owner marks, filler, replay and verdict.
5. **The tests §4 calls for**, each its own PR, each shrinking the
   uncovered list.

## 6. Open questions

- The weak-symbol init on every platform we build (mac, gnu and musl
  Linux), and in the reader wheel, which compiles the amalgamation on
  its own.
- One slow invocation for census and coverage together, or two simpler
  ones.
- How the nightly job reports: a red run, or an issue it keeps current.
- Doltlite stores in replay: the filler must be committed on the copy,
  and a statement that reads `dolt_diff` between two named commits must
  still find them.
