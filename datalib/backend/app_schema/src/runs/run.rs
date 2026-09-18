// One row per run of the pipeline.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "runs", primary_key = "run_id")]
pub struct RunRow {
    /// A UUID the runner mints, or the job id when the app started the
    /// run — the same string every other table here keys on.
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at_utc: String,
    /// UTC; `None` while the run is going, or if it died.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at_utc: Option<String>,
    /// The runner's own offset when it stamped the two above (`+02:00`).
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// The commit the runner was built from (`datalib_runs::git_hash`),
    /// when it knew: what a line's file and line number are relative to.
    #[col(sql = "VARCHAR(64)")]
    pub git_hash: Option<String>,
}
