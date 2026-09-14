// One row per step per run: what it is doing, or did.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "step_runs", primary_key = "run_id, step")]
pub struct StepRunRow {
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    #[col(sql = "VARCHAR(255)")]
    pub step: String,
    /// `pending` or `running` while live, else the terminal status the
    /// scheduler gave it — its `RunState`, which this crate does not
    /// name: "not one of the two live words" is all a reader needs.
    #[col(sql = "VARCHAR(32)")]
    pub state: String,
    /// Invocations so far this run; 0 before the first.
    #[col(sql = "INT")]
    pub attempt: i64,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at: Option<String>,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at: Option<String>,
    #[col(sql = "TEXT")]
    pub error: Option<String>,
    /// The step's own words: "conversations.list", "3 of 9 channels".
    #[col(sql = "TEXT")]
    pub msg: Option<String>,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub updated_at: String,
    /// The runner's offset when it stamped this row.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
