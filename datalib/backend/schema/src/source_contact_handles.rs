// The handles each `source_contacts` row ties to its person — the lookup
// a chip makes. See `source_contacts.rs`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(
    table = "source_contact_handles",
    primary_key = "markdown_uuid, contact_key, handle"
)]
pub struct SourceContactHandleRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    #[col(sql = "VARCHAR(256)")]
    pub contact_key: String,
    #[col(sql = "VARCHAR(256)")]
    pub handle: String,
}
