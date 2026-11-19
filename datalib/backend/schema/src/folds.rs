// The folds of a rendered document: where a `<details>` opens around a run
// of its sections and where it closes, with the class and summary it is
// drawn with. Kept apart from `sections` and `drawings` because a fold's
// piece lives in either, as layout or as text search reads, and the
// viewer draws a fold around the pieces it holds rather than reading it
// back out of their markup. Held by the render store alone.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One section that opens or closes a fold, carried by its document. A
/// row with a `class` opens a fold, drawn `<details class="{class}">` with
/// `summary` as its `<summary>`; one without closes the fold it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "folds", primary_key = "markdown_uuid, seq")]
pub struct FoldRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// Its place in the document, as in `sections`.
    #[col(sql = "INTEGER")]
    pub seq: i64,
    #[col(sql = "TEXT")]
    pub class: Option<String>,
    /// The summary as drawn, HTML the renderer escaped.
    #[col(sql = "TEXT")]
    pub summary: Option<String>,
}
