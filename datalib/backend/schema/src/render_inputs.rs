// What each rendered bucket was rendered from: one row per raw row a
// bucket's render asked for, found or not. The reverse of a provider's
// bucket query — a changed raw row names the buckets that read it — and
// the deletion record: a bucket whose inputs are gone re-renders to
// nothing, and its documents go with it. See
// docs/dev/data_architecture_parse_and_render.md.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(
    table = "render_inputs",
    primary_key = "bucket_key, input_table, input_id"
)]
pub struct RenderInputRow {
    /// The unit the provider loads and renders — a conversation, a
    /// thread, a PR, a page. Opaque to the driver; the same string the
    /// provider is handed back as a stale bucket.
    #[col(sql = "VARCHAR(256)")]
    pub bucket_key: String,
    /// A table of the raw store, bare name.
    #[col(sql = "VARCHAR(64)")]
    pub input_table: String,
    /// That table's primary key as text — a composite key's columns in
    /// `pragma_table_info` order, joined by `|`.
    #[col(sql = "VARCHAR(256)")]
    pub input_id: String,
}

/// The index the reverse lookup reads through. Beside the derived DDL
/// rather than in it: `PortableTable` emits one `CREATE TABLE` per row
/// type and nothing else.
pub const INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS render_inputs_by_input ON render_inputs (input_table, input_id)";
