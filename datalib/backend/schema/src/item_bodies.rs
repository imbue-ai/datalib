// What each item says, kept apart from how it is drawn, so the viewer, an
// export and the search index can each draw it their own way
// (docs/dev/plans/messages_not_documents.md §"Store the body, format on
// read"). Held by the render store alone; the grid index never copies it.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One item's body, carried by the document the item is in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "item_bodies", primary_key = "markdown_uuid, uuid")]
pub struct ItemBodyRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// The item's row, and its section's `data-section-uuid`.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    #[col(sql = "TEXT")]
    pub body: String,
    /// The body is still the item's whole section, header and wrapper
    /// included, as drawn by a renderer that has not split them off yet.
    /// What reads it draws nothing around it.
    #[col(sql = "INTEGER")]
    pub framed: bool,
}
