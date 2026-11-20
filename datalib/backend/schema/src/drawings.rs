// The layout between a rendered document's items, piece by piece: its
// front matter and title, a `<details>` opener, a line saying a thread is
// empty. Every item's piece is drawn from what `sections` keeps of it
// instead. With `sections` it spells the document byte for byte. Held by
// the render store alone.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One piece of layout, carried by its document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "drawings", primary_key = "markdown_uuid, seq")]
pub struct DrawingRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// Its place in the document, as in `sections`.
    #[col(sql = "INTEGER")]
    pub seq: i64,
    /// The piece as drawn.
    #[col(sql = "TEXT")]
    pub md: String,
}
