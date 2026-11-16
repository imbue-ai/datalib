// What a row or a container is filed under besides the container that
// holds it: an email thread under each of its labels, a photo under its
// keywords. Carried by the document that names it, like `nodes`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// A row or a container filed under a container that does not hold it:
/// an email thread under each of its labels, a photo under a keyword.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(
    table = "filings",
    primary_key = "markdown_uuid, node_uuid, under_uuid"
)]
pub struct FilingRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    #[col(sql = "VARCHAR(96)")]
    pub node_uuid: String,
    #[col(sql = "VARCHAR(96)")]
    pub under_uuid: String,
}

impl FilingRow {
    pub fn new(
        markdown_uuid: impl Into<String>,
        node_uuid: impl Into<String>,
        under_uuid: impl Into<String>,
    ) -> Self {
        FilingRow {
            markdown_uuid: markdown_uuid.into(),
            node_uuid: node_uuid.into(),
            under_uuid: under_uuid.into(),
        }
    }
}
