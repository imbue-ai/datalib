# `_datalib_meta` and the shape check

Every store Datalib writes carries one small table, `_datalib_meta`,
that says which build wrote it and what shape it is in. This crate
writes it, reads it, and holds the one check every reader makes before
it reads a row.

**A store is read in this build's shape, or it is refused.** Nothing
reads around a table, a column or an index an older build left out.
Before anything reads, the launch's prepare pass brings each store to
this build's shape, or marks its step as needing a rerun, which the
loop then runs while the app shows an upgrade screen. AGENTS.md §"No
fallbacks, no workarounds: one way to do it" is the rule this serves.

## What a store records

| key | what it is |
|---|---|
| `datalib_version`, `git_hash`, `doltlite_version` | the build that last opened it as its owner |
| `schema_hash` | blake3 over the DDL of its tables |
| `index_hash` | blake3 over the indexes its owner keeps apart from its tables; absent when every index is in `schema_hash` |
| `schema_version` | its place on its owner's migration ladder (`ladder.rs`) |
| `shared_schema_version` | its place on the framework's ladder for the tables every store shares, `problems` among them (`datalib_problems::SHAPE_VERSION`) |
| `store_kind` | which of Datalib's stores it is (`StoreKind`) |

The last five make a store's **shape** (`Shape`: its tables, its
indexes and its two ladder positions). A file has one shape: the
keyword index's tables are part of the grid index's, and the inference
cache's chunk tables are part of the cache's `index_hash`, so a change
to them drops the chunks, which are cut again, and keeps the vectors.

The inference cache's shape has one part `_datalib_meta` does not hold:
its page size, which the file's own header records
(`search_index::cache::PAGE_SIZE`). Its readers refuse any other size as
an `OtherShape`, and prepare rewrites the file in it, keeping every row.
Refusing is what lets prepare rewrite it: SQLite changes a page size only
when no other connection has the file open, and the search applet opens
the cache as soon as the app starts.

Indexes are recorded apart from tables so that adding one costs
creating it rather than rebuilding the store. The prepare pass creates
it, by opening the store as its owner, when a store's tables are this
build's and only its indexes are not. Because the shape includes the
indexes, a statement can name the index it relies on (`INDEXED BY`).
Opening the grid index as its owner also drops each index this build
does not make, or makes over other columns
(`grid_index::drop_other_indexes`). No other store does: there,
`CREATE INDEX IF NOT EXISTS` leaves an index as it was under a shape that
says it is new, so one whose columns change takes a new name.

## The check

`require(store, step, meta, want)` compares the shape a store records
with the one this build writes, and refuses anything else as an
`OtherShape`. That covers a store in an older shape, one in a newer
shape, and one that records no shape at all. The refusal names the
store and the step to run; `step_of(store)` names the `<group>/<function>`
folder a store sits in, which is its step for most stores.
`other_shape(e)` finds a refusal anywhere in an error's chain, for a
caller that reports it rather than failing on it.

Each kind of store has one function that says the shape it is written
in, and one reader-open that checks it:

| store | shape | reader |
|---|---|---|
| raw store | a new store made in scratch (`datalib_step::prepare::raw_shape`) | the render step, before any provider reads (`render::require_raw_shape`) |
| render store | `indexed_markdown::shape` | `IndexedMarkdownStore::open_for_reading` |
| grid index | `grid_index::shape` | `DoltRepo::pinned`, inside each read; `grid_index::open_for_reading` |
| search index (in the grid index's file) | `grid_index::shape` | `search_index::open` in the grid's crate |
| inference cache | `search_index::cache::shape` | `search_index::Reader::open` |
| time-series store, its index | `datalib_etl_time_series::store::shape` | `datalib_etl_time_series::store::open_reader` |
| occurrences | `occurrences::store::shape` | (read only by its own step) |
| contacts | `doltlite_raw::curated_shape` | `ContactsReader::open` |
| a step's `problems.sqlite` | `StepFile::shape` | `datalib_step_problems` reads `shared_schema_version`, of every store that holds `problems`; a render store without `entity_items` is in another shape too |
| run store (`system/runs/runs.sqlite`) | `datalib_runs::store::shape` | `datalib_runs::store::open_reader` |

Once a store passes the check, every table and column this build reads
is in it. A "no such table" or "no such column" after that means the
store is damaged, not old, and fails as one.

The other side, `guard::refuse_if_newer`, keeps a build from writing a
store a newer line of Datalib wrote, and refuses it at prepare
(`failure: incompatible`).
