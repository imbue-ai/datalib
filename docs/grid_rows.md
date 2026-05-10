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
   move. Codegen propagates the change to Python (writer side), Rust
   (reader side), and TypeScript (consumer side). Drift between the
   three has historically been a recurring bug source.
2. **One query path on the backend.** `frankweiler/backend/core/src/db.rs`
   is now a single `SELECT … FROM grid_rows WHERE …` plus a row mapper.
   Adding a provider doesn't add a `push_*` function; it adds rows to the
   table at ingest time. The query/filter/sort logic stays put.
3. **No "discover at query time" joins.** Per-message rows already carry
   the parent conversation's name, account, project — so the grid renders
   straight off the projection without join cost.

## Source of truth

`schemas/grid_rows.schema.json` defines the row shape. Each property
carries:

- `x-sql-type` — portable DDL type (the SQL subset shared by Dolt, MySQL,
  and SQLite).
- `x-mapping` — per-provider expression that documents how the column is
  derived. Read by humans, not by code; lives next to the column it
  describes so it can't drift.
- `description` — emitted as Rust `///`, Python field docstring, and
  TypeScript JSDoc.

`schemas/codegen.py` produces:

| Output                                                  | Used by                                          |
|---------------------------------------------------------|--------------------------------------------------|
| `src/ingest/generated_grid_rows.py`                     | `src/ingest/grid_rows.py` (writer)               |
| `frankweiler/backend/schema/src/generated/grid_rows.rs` | `frankweiler/backend/core/src/db.rs` (reader)    |
| `frankweiler/ui/src/generated/grid_rows.ts` (genrule)   | future TypeScript consumers                      |

The generated `DDL` constant is used both at ingest time
(`ensure_schema`) and from the `dump.sql` portable-DDL emitter.

## Producer side: `src/ingest/grid_rows.py`

`populate_grid_rows(conn, anthropic, openai, slack)` is invoked by
`src/ingest/ingest.py` after each provider has been parsed and merged in
memory, and **before** the Dolt commit. It:

1. Drops + recreates `grid_rows` (it's the only SQL artifact the ingest
   pipeline writes, so dropping is always safe — and it lets schema
   changes take effect without explicit migrations).
2. Walks each provider's merged `Parsed*` dataclass, builds `_Row`
   instances, and emits one `executemany` INSERT per ingest.

Provider-specific Dolt tables don't exist anymore: the parsed dataclasses
ARE the source of truth for both QMD rendering and grid_rows population.
Re-population strategy is full delete + reinsert per ingest. Cheap at
our scale (~5k rows), avoids row-level UPSERT complexity, and guarantees
consistency with any column-mapping changes.

## Consumer side: `frankweiler/backend/core/src/db.rs`

`grid_rows_with_conn` builds a `WHERE` clause from `ParsedQuery`
(account/project/before/after/free-text) plus a kind clause from
`q.resolved_type` (chat: vs message:), then issues a single SELECT
against `grid_rows` ordered by `when_ts` ASC with chat rows tie-breaking
ahead of their messages. The row mapper translates each row into a
`SearchRow` for the HTTP API.

The unit test in `db.rs` builds an in-memory SQLite using the
codegen-emitted DDL — the same DDL the production ingester uses — so
the test sees the same column set as production.

## Adding a column

1. Edit `schemas/grid_rows.schema.json`. Add the property; include an
   `x-mapping` entry per provider so future-you knows where the value
   comes from.
2. Run codegen (see `README.md` → "Regenerating the cross-language types").
3. Add the column to `_Row` in `src/ingest/grid_rows.py` and to every
   row builder. Update the `INSERT` tuple accordingly.
4. Update `db.rs`'s `SELECT`, the destructured row, and `SearchRow` in
   `search.rs` if the column should reach the API.
5. Add it to the column manifest in `frankweiler/backend/http/src/lib.rs`
   if the grid should display it.
6. Re-bake snapshots:
   ```sh
   bazelisk build //tests/fixtures:ingested_tng
   uv run pytest tests/test_snapshots.py --snapshot-update
   ```

## Adding a provider

1. Land the per-provider tables and ingester in
   `src/ingest/providers/<p>/`.
2. Add a row-builder function to `src/ingest/grid_rows.py` that walks
   the new tables and emits `_Row`s with the right `provider`/`kind`/
   `source_label` strings.
3. Append it to the `builders` tuple in `populate_grid_rows`.
4. Add the source label to the consuming bits as needed (icon
   resolution, etc.) — but the query path itself does not change.

## Why this isn't a materialized view

We considered Dolt-side triggers / views. Two reasons we rolled our own:

- The mapping logic isn't always pure SQL. `_extract_model_from_raw`
  pulls a JSON field with a regex; `_bump_micros` synthesizes a
  per-block timestamp. Doing this in Python next to the rest of the
  ingester keeps the mapping legible.
- We ship a portable `mirror.sqlite` to the Rust backend, not Dolt
  itself. Anything we materialize has to live in plain DDL the SQLite
  loader will accept — so a generated table works; a view that depends
  on Dolt-specific features doesn't.
