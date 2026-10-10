// Free-space timeseries: how much the data root's disk had left, sampled
// over time. Lives in the usage store beside `disk_usage`, and like it is
// never committed: the rows are the history.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One look at the volume the data root lives on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "disk_free", primary_key = "measured_at_utc")]
pub struct DiskFreeRow {
    #[col(sql = "VARCHAR(40)")]
    pub measured_at_utc: String,
    /// The server's offset when it stamped `measured_at_utc`.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// What an unprivileged process could still write (`statvfs`'s
    /// `f_bavail`).
    #[col(sql = "BIGINT")]
    pub available_bytes: i64,
    #[col(sql = "BIGINT")]
    pub total_bytes: i64,
}
