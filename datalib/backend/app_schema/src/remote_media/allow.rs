// One decision to load remote media. `(scope, key)` is unique: asking
// twice is one row.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "remote_media_allow", primary_key = "allow_uuid")]
pub struct RemoteMediaAllowRow {
    #[col(sql = "VARCHAR(36)")]
    pub allow_uuid: String,
    /// An [`super::AllowScope`] spelling, bound as text.
    #[col(sql = "VARCHAR(16)")]
    pub scope: String,
    #[col(sql = "TEXT")]
    pub key: String,
    #[col(sql = "VARCHAR(40)")]
    pub created_at_utc: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
