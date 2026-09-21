// One row per process that took part: a run of the runner, each
// attempt of each of its steps, a launch of the app's server, or a
// page of the app open in a browser tab (the server records it). What
// a line's file and line number are relative to lives here, once,
// rather than on every line — and so does how a process ended.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "processes", primary_key = "process_id")]
pub struct ProcessRow {
    /// A UUID: minted by the process itself when it opens the store, by
    /// the runner for a step it spawns, or by a page for itself.
    #[col(sql = "VARCHAR(64)")]
    pub process_id: String,
    /// A [`super::Process`] word: which program this was.
    #[col(sql = "VARCHAR(16)")]
    pub process: String,
    /// The run this process belonged to — the runner's own, or the one
    /// its step ran in. `None` for the server and a page: they run
    /// between runs.
    #[col(sql = "VARCHAR(64)")]
    pub run_id: Option<String>,
    /// For a step's process: which step, and which attempt of it.
    #[col(sql = "VARCHAR(255)")]
    pub step: Option<String>,
    #[col(sql = "INT")]
    pub attempt: Option<i64>,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at_utc: String,
    /// UTC; `None` while the process is going, or if it died without
    /// anyone recording the end.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at_utc: Option<String>,
    /// How it ended, when something watched it end: the runner records
    /// each step's. `exit_code` when the process exited on its own;
    /// `signal` when a signal ended it; neither for a process that
    /// records itself, which cannot see its own end.
    #[col(sql = "INT")]
    pub exit_code: Option<i64>,
    #[col(sql = "INT")]
    pub signal: Option<i64>,
    /// The process's own offset when it stamped the above (`+02:00`).
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// The commit the process was built from (`datalib_runs::git_hash`),
    /// when known: a step's is the runner's when it runs the built-in
    /// step program, and nothing for a custom command.
    #[col(sql = "VARCHAR(64)")]
    pub git_hash: Option<String>,
}
