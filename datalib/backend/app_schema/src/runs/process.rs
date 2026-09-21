// One row per datalib process that wrote to the store: a run of the
// runner, or a launch of the app's server. What a line's file and line
// number are relative to lives here, once, rather than on every line.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "processes", primary_key = "process_id")]
pub struct ProcessRow {
    /// A UUID the process mints for itself when it opens the store.
    #[col(sql = "VARCHAR(64)")]
    pub process_id: String,
    /// A [`super::Process`] word: which program this was.
    #[col(sql = "VARCHAR(16)")]
    pub process: String,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at_utc: String,
    /// The process's own offset when it stamped the above (`+02:00`).
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// The commit the process was built from (`datalib_runs::git_hash`),
    /// when it knew.
    #[col(sql = "VARCHAR(64)")]
    pub git_hash: Option<String>,
}
