// The row-struct write contract, shared by both schema families.
//
// This trait started in `datalib_etl::bulk`, where the raw-download
// derives (`WirePayloadRow`, `RawTable`, `CasEdgeRow`) emit impls of
// it. The render-schema derive (`PortableTable`) could not: `datalib_etl`
// depends on `datalib_schema`, so a `datalib_schema` struct implementing
// a `datalib_etl` trait is a dependency cycle. That is the whole reason
// the render schema had no generated write path and `grid_index`
// hand-wrote its INSERTs.
//
// Moving the trait down here — a leaf both sides can see — lets one
// contract serve both. `datalib_etl::bulk` re-exports it, so every
// existing `datalib_etl::bulk::BulkUpsertable` path still resolves and
// no provider code changed.
//
// The bulk *helpers* (`bulk_upsert_in_tx` and friends) stay in
// `datalib_etl`: they carry the bookkeeping-table and wire-tape
// concerns, which are raw-store ideas the render schema has no use for.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Row-struct contract that lets the generic [`bulk_upsert_in_tx`]
/// helper write a batch into a table.
///
/// **The universal entity-table shape.** Most raw entity tables are
/// `(id, …typed_columns, payload)`:
///
///   - `id` — TEXT primary key, the upstream identifier (or a
///     UUIDv5 synthesized from upstream-stable components when no
///     stable id exists).
///   - `…typed_columns` — zero or more writer-supplied fields that
///     aren't in the payload (synthesized-PK components, FK
///     references, namespace discriminators). Plain `?` binds.
///   - `payload` — JSON text, stored as JSONB via `jsonb(?)` on
///     write. The full upstream message, losslessly transcoded if
///     necessary (see `docs/dev/data_architecture_ingestion.md`
///     §"Wire-fidelity of the raw store").
///
/// **Some tables have no payload column** — N:M edge / attachment
/// tables in particular (e.g. Signal's `chat_item_attachments`)
/// just record the join. Set [`Self::PAYLOAD_COLUMN`] to `None` for
/// those; the helper will emit a payload-less INSERT and
/// `bind_into` should not bind anything past the typed columns.
///
/// **Where impls live.** By convention, the row struct and its
/// `BulkUpsertable` impl live in the provider's `schema_raw.rs`,
/// right next to the matching `CREATE TABLE` DDL constant, so that
/// the rust struct's fields and the SQL columns are visibly aligned
/// at the same vertical position in the file.
///
/// **Required correspondence.** [`Self::TYPED_COLUMNS`] must list
/// the non-PK, non-payload columns in the same order as
/// [`Self::bind_into`] binds them, and that order must match the
/// DDL's column declarations between `id` and the payload column
/// (if any). [`Self::bind_into`] binds id first, then each typed
/// column in order, then the payload as a JSON text string (when
/// [`Self::PAYLOAD_COLUMN`] is `Some`). Mismatch → mis-binding at
/// runtime.
///
/// **One writer per row.** Per
/// `docs/dev/data_architecture_ingestion.md` §"One writer per row," the
/// ON CONFLICT clause is uniform across all tables: every non-PK
/// column is set to `excluded.<col>`. There is no per-table or
/// per-column override.
pub trait BulkUpsertable: Sync {
    /// Target table name. Must match the DDL.
    const TABLE: &'static str;

    /// Name of the single-column primary key, used as the `ON
    /// CONFLICT(<id>)` target and as the first column in the INSERT.
    /// Almost always `"id"` (the universal raw-entity PK name); a few
    /// tables key on a different column (e.g. an mbox cursor keyed on
    /// `path`). N:M join tables synthesize a single `id` from their
    /// composite components rather than overriding this, so the
    /// conflict target stays one column everywhere.
    const ID_COLUMN: &'static str = "id";

    /// Non-PK, non-payload columns, in bind order. These bind as
    /// plain `?`. Empty slice for tables that are just `(id, payload)`
    /// (e.g. Signal's `account`) or just `(id)` plus typed columns
    /// with no payload (e.g. `chat_item_attachments`).
    const TYPED_COLUMNS: &'static [&'static str];

    /// JSON payload column name, bound as `jsonb(?)`. Almost always
    /// `Some("payload")`. Set to `None` for tables that have no
    /// payload column (e.g. attachment / N:M edge tables that just
    /// record a join).
    const PAYLOAD_COLUMN: Option<&'static str> = Some("payload");

    /// PK value for this row. The PK column is always named `id` in
    /// every raw entity table (see
    /// `docs/dev/data_architecture_ingestion.md` §"Object identity").
    fn id(&self) -> &str;

    /// Bind the id, then each value in [`Self::TYPED_COLUMNS`] order,
    /// then (if [`Self::PAYLOAD_COLUMN`] is `Some`) the payload as a
    /// JSON text string. The helper has already emitted matching
    /// placeholders (`?` for id and typed columns, `jsonb(?)` for
    /// payload); this method just calls `q.bind(...)` once per column.
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments>;
}
