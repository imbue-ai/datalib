// One URL fetched into the download CAS: the bytes are at
// `system/remote_media/<sha256>`, and this row is what they are.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "remote_media", primary_key = "url")]
pub struct RemoteMediaRow {
    #[col(sql = "TEXT")]
    pub url: String,
    #[col(sql = "VARCHAR(64)")]
    pub sha256: String,
    /// `type/subtype` as the host sent it, lowercased, no parameters.
    #[col(sql = "VARCHAR(128)")]
    pub content_type: String,
    #[col(sql = "BIGINT")]
    pub byte_size: i64,
    #[col(sql = "VARCHAR(40)")]
    pub fetched_at_utc: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
