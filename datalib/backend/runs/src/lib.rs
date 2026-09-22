//! The run store: one plain-SQLite file per data root that the runner
//! writes and anything can read. Holds what every run did — each step's
//! state, its log lines, and its metrics — across runs, until retention
//! removes the old ones, and the app server's own log between them. The
//! tables are `app_schema::runs`; this crate is the writers and the
//! reader over them, and the tracing layer that feeds a writer.

pub mod query;
pub mod store;
pub mod tracing_layer;

pub use app_schema::runs::{
    LogLevel, LogRow, MetricRow, MetricSampleRow, Process, ProcessRow, RunRow, StepRunRow,
    StorePart, Stream,
};
pub use datalib_runtime::build_id::{
    git_hash, git_hash_and_origin, GitHashOrigin, GIT_HASH_ENV, NO_GIT_HASH_ADVICE,
};
pub use query::{log_query, LogQuery, QueryError};
pub use store::{
    canonical_labels, latest_metric, log_after, log_line, new_process_id, open_or_create, process,
    processes, runs, snapshot, snapshot_of, versions, LogLine, LogSink, ProcessLogWriter,
    RunWriter, Snapshot,
};
pub use tracing_layer::{default_filter, filter_at, StoreLayer, DEFAULT_LEVEL};

use std::path::{Path, PathBuf};

/// One line, once the subscriber is up, saying which commit this
/// process records on its rows and where it read it — or, when it has
/// none, what that costs and how to give it one. A dev binary run
/// straight out of bazel-bin has the file `//datalib/backend:bin`
/// stages; a launcher sets the variable; a release carries the file.
pub fn log_build_commit(found: Option<&(String, GitHashOrigin)>) {
    match found {
        Some((hash, origin)) => {
            tracing::info!(commit = %hash, "build commit read from {}", origin.describe())
        }
        None => tracing::warn!("{NO_GIT_HASH_ADVICE}"),
    }
}

/// Where the store lives under a data root: `system/runs/runs.sqlite`,
/// as `datalib_runtime::layout` places it.
pub fn runs_path(data_root: &Path) -> PathBuf {
    datalib_runtime::layout::runs_db(data_root)
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
pub const SCHEMA_VERSION: i32 = 9;

/// The indexes, beside the tables' own DDL. `log.seq` is the rowid, so
/// a reader tailing "everything after N" needs no timestamp arithmetic;
/// the index is for narrowing that to one run and step.
pub const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS log_by_run_step ON log (run_id, step, seq)",
    // For retention's "no line names this process".
    "CREATE INDEX IF NOT EXISTS log_by_process ON log (process_id)",
];
