# `grid_rows` — the union table behind the grid

The AG Grid in `frankweiler/ui` shows one row per "displayable thing" in
the mirror: chat conversations, individual messages, content blocks
(tool_use / tool_result / thinking), Slack threads, Slack messages.
Rather than have the Rust backend dispatch per-provider — five queries
unioned in code, with five distinct row builders — every ingest writes a
denormalized projection into a single Dolt table, **`grid_rows`**, and
the backend reads it with one query.

## Why a union table

Three forces pushed us this direction:

1. **One source of truth for column semantics.** When the grid grows a
   new column (`channel`, `slack_link`, …), exactly one schema needs to
   move. Codegen propagates the change to Rust (both writer and reader
   sides) and TypeScript (consumer side). Drift has historically been
   a recurring bug source.
2. **One query path on the backend.** `frankweiler/backend/core/src/db.rs`
   is now a single `SELECT … FROM grid_rows WHERE …` plus a row mapper.
   Adding a provider doesn't add a `push_*` function; it adds rows to the
   table at ingest time. The query/filter/sort logic stays put.
3. **No "discover at query time" joins.** Per-message rows already carry
   the parent conversation's name, account, project — so the grid renders
   straight off the projection without join cost.

## Source of truth

The hand-written `GridRow` struct in
`frankweiler/backend/schema/src/grid_rows.rs` defines the row shape — it
is the single source of truth, with no codegen step. Each field carries:

- `#[col(sql = "…")]` — portable DDL type (the SQL subset shared by Dolt
  and MySQL). Nullability is inferred from `Option<T>`.
- `#[derived(name = "…", sql = "…")]` — a column computed at grid-index time
  (e.g. `when_ts_utc` / `when_offset`, derived from `when_ts`). Present in
  the DDL but absent from the struct.
- doc comment — the per-provider mapping documenting how the column is
  derived, kept next to the field so it can't drift.

`#[derive(PortableTable)]` (in `frankweiler/backend/etl/macros`) produces
from the struct the `DDL`, `COLUMNS`, and `TABLES` module consts. The
`DDL` constant is used at grid-index time (`init_schema` in
`etl/src/grid_index.rs`) and from the `dump.sql` portable-DDL emitter.

## Producer side: per-provider `render/grid_rows.rs`

Each provider crate under `frankweiler/backend/etl/providers/<p>/`
emits `*.grid_rows.json` sidecars next to its rendered markdown. The
grid_index step (`datalib-step grid_index`; `build_grid_index` in
`frankweiler/backend/etl/src/grid_index.rs`) walks every sidecar under
`<root>/<stanza>/rendered_md/`, upserts each conversation's row set
into Dolt, and stamps the corresponding `documents` row with the
`row_set_hash` used to skip unchanged re-renders next time.

## Consumer side: `frankweiler/backend/core/src/dolt_repo.rs`

`DoltRepo::search` builds a `WHERE` clause from `ParsedQuery`
(account/project/before/after/free-text) plus a kind clause from
`q.resolved_type` (chat: vs message:), then issues a single SELECT
against `grid_rows` ordered by `when_ts` ASC with chat rows tie-breaking
ahead of their messages. The row mapper translates each row into a
`SearchRow` for the HTTP API.

## Adding a column

1. Add the field to the `GridRow` struct in
   `frankweiler/backend/schema/src/grid_rows.rs`, with a `#[col(sql = "…")]`
   portable type and a doc comment carrying the per-provider mapping so
   future-you knows where the value comes from.
2. Add the column to each per-provider `render/grid_rows.rs`
   `GridRow` builder.
3. Update `dolt_repo.rs`'s `SELECT`, the destructured row, and
   `SearchRow` in `search.rs` if the column should reach the API.
4. Add it to the column manifest in `frankweiler/backend/http/src/lib.rs`
   if the grid should display it.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

## Adding a provider

1. Land a new crate under `frankweiler/backend/etl/providers/<p>/`
   with a `render/grid_rows.rs` emitting `GridRow`s with the right
   `provider` / `kind` / `source_label` strings, and a renderer that
   writes the `*.grid_rows.json` sidecars alongside its markdown.
2. Wire the new crate into `datalib-step`: add it to the deps of
   `frankweiler/backend/datalib_step` and to the dispatch table in
   `frankweiler/backend/datalib_step/src/dispatch.rs`, then declare its
   download/render step pair in the config. The grid_index step picks
   up its sidecars with no further wiring.
3. Add the source label to the consuming bits as needed (icon
   resolution, etc.) — but the query path itself does not change.

## Why this isn't a materialized view

We considered Dolt-side triggers / views. The mapping logic isn't
always pure SQL — timestamps get bumped to synthesize per-block
ordering, JSON fields get parsed out of raw payloads — so a generated
table built next to the rest of the translator code keeps the mapping
legible and avoids depending on Dolt-specific features.
