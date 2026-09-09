//! The row-struct write contract: one trait, implemented by every row
//! struct that a bulk INSERT/UPSERT helper can write.
//!
//! Its own crate, with `sqlx` as its only dependency, because both
//! schema families (`datalib_schema`'s render tables and `app_schema`'s
//! server tables) implement it and every provider's *download* side
//! needs it. Left in `datalib_schema`, this fifty-line trait was the
//! only thing making downloaders rebuild when a `grid_rows` column
//! moved. Keep the dependency list empty but for `sqlx`.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Row-struct contract that lets the generic [`bulk_upsert_in_tx`]
/// helper write a batch into a table.
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
