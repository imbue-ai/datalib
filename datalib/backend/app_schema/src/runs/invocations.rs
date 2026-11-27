// One row per invocation: one process the loop started for one step, one
// attempt of it (a retry is the next invocation). What it was
// started with, how it is going and how it ended. The requests it served
// are in `invocation_requests`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "invocations", primary_key = "invocation_id")]
pub struct InvocationRow {
    /// A UUID the loop mints as it starts the step, the one the step is
    /// handed as `DATALIB_DAG_INVOCATION_ID`.
    #[col(sql = "VARCHAR(64)")]
    pub invocation_id: String,
    #[col(sql = "VARCHAR(255)")]
    pub step: String,
    /// The `[[groups]]` entry the step is filed under; `None` for a step
    /// outside any group.
    #[col(sql = "VARCHAR(255)")]
    pub group_id: Option<String>,
    /// The verb appended to its command (`prepare`, `reset`, `compact`,
    /// `collect`) for one a job started; `None` for a step's own run.
    #[col(sql = "VARCHAR(16)")]
    pub verb: Option<String>,
    /// `running` while it is, else how it ended: the runner's `RunState`
    /// word, which this crate does not name (`datalib_runs::LiveState`).
    #[col(sql = "VARCHAR(32)")]
    pub state: String,
    /// Attempts so far; a retry counts up.
    #[col(sql = "INT")]
    pub attempt: i64,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at_utc: String,
    /// UTC; `None` while it runs, or if the loop running it died before
    /// the next loop closed it.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at_utc: Option<String>,
    /// The `DATALIB_DAG_NOW` it was handed, as handed.
    #[col(sql = "VARCHAR(40)")]
    pub now: Option<String>,
    /// The step's definition when it started (`Graph::fingerprints`).
    #[col(sql = "VARCHAR(64)")]
    pub fingerprint: Option<String>,
    /// What it was started against, as `DATALIB_READS` says it: a JSON
    /// object of input path to version.
    #[col(sql = "TEXT")]
    pub reads: Option<String>,
    #[col(sql = "TEXT")]
    pub error: Option<String>,
    /// The step's own words: "conversations.list", "3 of 9 channels".
    #[col(sql = "TEXT")]
    pub msg: Option<String>,
    #[col(sql = "INT")]
    pub exit_code: Option<i64>,
    #[col(sql = "INT")]
    pub signal: Option<i64>,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub updated_at_utc: String,
    /// The runner's offset when it stamped this row.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
