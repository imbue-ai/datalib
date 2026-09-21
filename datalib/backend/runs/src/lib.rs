//! The run store: one plain-SQLite file per data root that the runner
//! writes and anything can read. Holds what every run did — each step's
//! state, its log lines, and its metrics — across runs, until retention
//! removes the old ones, and the app server's own log between them. The
//! tables are `app_schema::runs`; this crate is the writers and the
//! reader over them, and the tracing layer that feeds a writer.

pub mod build_id;
pub mod query;
pub mod store;
pub mod tracing_layer;

pub use app_schema::runs::{
    LogLevel, LogRow, MetricRow, MetricSampleRow, Process, ProcessRow, RunRow, StepRunRow,
    StorePart, Stream,
};
pub use build_id::{git_hash, GIT_HASH_ENV};
pub use query::{log_query, LogQuery, QueryError};
pub use store::{
    canonical_labels, latest_metric, log_after, open_or_create, runs, snapshot, snapshot_of,
    versions, LogLine, LogSink, ProcessLogWriter, RunWriter, Snapshot,
};
pub use tracing_layer::{StoreLayer, DEFAULT_LOG_FILTER};

use std::path::{Path, PathBuf};

/// Where the store lives under a data root.
pub const RUNS_REL_PATH: &str = "system/runs.sqlite";

pub fn runs_path(data_root: &Path) -> PathBuf {
    data_root.join(RUNS_REL_PATH)
}

/// The two states the store itself names. Every other value of
/// [`StepRunRow::state`] is a terminal status minted by whoever writes the
/// store — the DAG runner's `RunState`, today — which this crate
/// deliberately does not enumerate: the scheduler's vocabulary is not its
/// business. "Not one of these two" is the whole of what it needs to know.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum LiveState {
    /// In the plan, not yet reached.
    Pending,
    /// Invoked, and still going.
    Running,
}

impl LiveState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<LiveState> {
        s.parse().ok()
    }
}

/// Whether a [`StepRunRow::state`] means the step is finished.
pub fn is_terminal(state: &str) -> bool {
    LiveState::parse(state).is_none()
}

/// How much history to keep. Both run limits apply; a run older than
/// `max_age_days` goes even when fewer than `max_runs` exist. The lines
/// outside any run — the server's own log — have their own two, both
/// shorter: at `debug` a server between syncs writes steadily, and a
/// month of that is a file nobody reads back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_runs: u32,
    pub max_age_days: u32,
    pub process_log_days: u32,
    pub process_log_lines: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_runs: 100,
            max_age_days: 30,
            process_log_days: 3,
            process_log_lines: 200_000,
        }
    }
}

/// Bumped whenever the tables change shape. A store carrying another
/// version is deleted and remade rather than migrated: nothing in it is
/// load-bearing, and a migration is code that would exist only to keep
/// old log lines.
pub const SCHEMA_VERSION: i32 = 8;

/// The indexes, beside the tables' own DDL. `log.seq` is the rowid, so
/// a reader tailing "everything after N" needs no timestamp arithmetic;
/// the index is for narrowing that to one run and step.
pub const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS log_by_run_step ON log (run_id, step, seq)",
    // For retention's "no line names this process".
    "CREATE INDEX IF NOT EXISTS log_by_process ON log (process_id)",
];
