// People as sources describe them (`NormalizedContact`, in
// `datalib_contact_schema`), carried by the document that saw them, the
// way a document carries its edges: a re-render replaces its own rows,
// and a person mentioned in many documents has a row in each. Both tables
// are a render store's; the grid index keeps each record once
// (`etl/render/src/grid_index/contact_records.rs`).

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "source_contacts", primary_key = "markdown_uuid, contact_key")]
pub struct SourceContactRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// The source's own id for the person: `NormalizedContact::key`.
    #[col(sql = "VARCHAR(256)")]
    pub contact_key: String,
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    /// The name the source prefers, for a reader that wants only that.
    #[col(sql = "TEXT")]
    pub name: Option<String>,
    /// How many items in this document the person wrote, and the newest
    /// one's stamp; zero and null for a source that is not a chat.
    #[col(sql = "INTEGER")]
    pub seen_items: i64,
    #[col(sql = "VARCHAR(64)")]
    pub last_seen_at: Option<String>,
    /// The whole `NormalizedContact`, as JSON, so the type can grow without
    /// a column per field.
    #[col(sql = "TEXT")]
    pub contact_json: String,
}
