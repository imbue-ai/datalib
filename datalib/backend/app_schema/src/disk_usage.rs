// Disk-usage timeseries: how many bytes each tree under the data root
// occupies, sampled over time.
//
// Written by `datalib-http`'s usage sampler (`datalib_http::usage`),
// which walks the root on a fixed tick and appends a row per series
// whose value moved. Two rules keep the table from growing without
// saying anything, and both live in the sampler rather than here:
//
//   * an unchanged measurement is not recorded — the series is a step
//     function, so a repeated value carries no information, and the
//     last row at or before an instant *is* the value at that instant;
//   * two rows for one series are never closer than five seconds apart.
//
// So this is a compacted timeseries, and any reader has to carry the
// last value forward rather than assume a fixed sampling interval.
//
// Its own doltlite file (`system/usage.doltlite_db`) for the usual
// reason: doltlite's working set is per file, so sharing one with the
// job queue would sweep whichever rows happened to be dirty into the
// other's commits. Nothing here is ever `dolt_commit`ed — the rows are
// the history, and a commit per sample would buy nothing and flood
// `dolt_log`.
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// The `path` value standing for the data root as a whole.
///
/// Every other row's `path` is a step id, which is the data-root-
/// relative tree that step writes. The root itself has no step and so
/// no id; `.` is the relative path that names it and cannot collide
/// with a step id (the loader refuses one that isn't a plain path
/// segment sequence).
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
