// Disk-usage timeseries: how many bytes each tree under the data root
// occupies, sampled over time.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// The `path` value standing for the data root as a whole.
pub const ROOT_PATH: &str = ".";

/// One measurement of one tree.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "disk_usage", primary_key = "path, measured_at")]
pub struct DiskUsageRow {
    /// Which tree this measures: a step id (the data-root-relative tree
    /// that step writes), or [`ROOT_PATH`] for the whole data root.
    #[col(sql = "VARCHAR(512)")]
    pub path: String,
    /// When the walk that produced this number finished (ISO-8601 with
    /// explicit local offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub measured_at: String,
    /// Total bytes under the tree, following no symlinks. A symlink is
    /// counted as its own (tiny) entry, never as what it points at —
    /// otherwise a cycle hangs the walk and a shared target is counted
    /// twice.
    #[col(sql = "BIGINT")]
    pub bytes: i64,
}
