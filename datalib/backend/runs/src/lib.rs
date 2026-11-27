//! The run store: one plain-SQLite file per data root that the runner
//! writes and anything can read. Holds what every invocation of a step
//! did — how it went, its log lines and its metrics — and which requests
//! it served, until retention removes the old ones, and the runner's and
//! the app server's own log. What an older build recorded per busy
//! period (a "run") stays readable as it was written. The tables are
//! `app_schema::runs`; this crate is the writers and the readers over
//! them, and the tracing layer that feeds a writer.

pub mod query;
mod redact;
pub mod store;
pub mod tracing_layer;

pub use app_schema::runs::{
    InvocationMetricRow, InvocationMetricSampleRow, InvocationRequestRow, InvocationRow, LogLevel,
    LogRow, MetricRow, MetricSampleRow, Process, ProcessRow, RunKind, RunRow, StepRunRow,
    StorePart, Stream,
};
pub use datalib_runtime::build_id::{
    git_hash, git_hash_and_origin, GitHashOrigin, GIT_HASH_ENV, NO_GIT_HASH_ADVICE,
};
pub use query::{key_names, log_query, log_values, LogCursor, LogQuery, QueryError};
pub use store::{
    after_every_write, canonical_labels, close_abandoned_invocations, invocations_of,
    latest_metric, latest_metrics, log_after, log_line, metric_by_invocation, metric_history,
    new_process_id, open_or_create, prepare_store, process, processes, requests_of_steps, runs,
    runs_of_steps, snapshot, snapshot_of, step_problem_lines, step_samples, versions, LogLine,
    LogSink, LoopWriter, MetricSample, MetricValue, ProcessLogWriter, ReportedIn, Snapshot,
    SnapshotStep, QUEUE_WINDOW,
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
/// [`InvocationRow::state`] (and of an older build's [`StepRunRow::state`])
/// is a terminal status minted by whoever writes the store — the DAG
/// runner's `RunState` — which this crate deliberately does not
/// enumerate: the scheduler's vocabulary is not its business. "Not one of
/// these two" is the whole of what it needs to know.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum LiveState {
    /// In an older build's plan, not yet reached. This build writes no
    /// pending row: an invocation's first row is a running one.
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

/// Whether a state word means the invocation (or an older run's step) is
/// finished.
pub fn is_terminal(state: &str) -> bool {
    LiveState::parse(state).is_none()
}

/// How much history to keep. Both limits apply: an invocation goes once
/// it is older than `max_age_days`, or once none of the newest `max_runs`
/// requests it served is still kept; an older build's run goes by the
/// same two. The lines about no invocation — the runner's and the
/// server's own log — have their own two, both shorter: at `debug` a
/// server between syncs writes steadily, and a month of that is a file
/// nobody reads back.
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

/// Bumped whenever the tables change shape. A store at an older version
/// gets the rungs above it in `store.rs`'s `UPGRADES` applied in order,
/// in place; one at a version with no rung, newer ones included, is
/// emptied and remade.
pub const SCHEMA_VERSION: i32 = 16;

/// The indexes, beside the tables' own DDL. `log.seq` is the rowid, so
/// a reader tailing "everything after N" needs no timestamp arithmetic;
/// the indexes are for narrowing that to one invocation, or one older
/// build's run and step.
pub const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS log_by_run_step ON log (run_id, step, seq)",
    // For retention's "no line names this process".
    "CREATE INDEX IF NOT EXISTS log_by_process ON log (process_id)",
    PROCESSES_BY_RUN,
    RUNS_BY_START,
    METRICS_BY_NAME,
    METRIC_SAMPLES_BY_NAME,
    LOG_BY_INVOCATION,
    PROCESSES_BY_INVOCATION,
    INVOCATIONS_BY_START,
    INVOCATIONS_BY_STEP,
    REQUESTS_BY_REQUEST,
    INVOCATION_METRICS_BY_NAME,
    INVOCATION_SAMPLES_BY_NAME,
];

/// For retention, which removes a run's processes with the run.
pub const PROCESSES_BY_RUN: &str =
    "CREATE INDEX IF NOT EXISTS processes_by_run ON processes (run_id)";

/// For the newest run, and for retention, which keeps the newest.
pub const RUNS_BY_START: &str =
    "CREATE INDEX IF NOT EXISTS runs_by_start ON runs (started_at_utc, run_id)";

/// For Manage's reads of a few metrics, on every refresh. Both keys lead
/// with `run_id`, so a read by name walked every run's series, and a read
/// of three metrics in one run walked every sample that run took.
pub const METRICS_BY_NAME: &str =
    "CREATE INDEX IF NOT EXISTS metrics_by_name ON metrics (name, step, labels)";
pub const METRIC_SAMPLES_BY_NAME: &str = "CREATE INDEX IF NOT EXISTS metric_samples_by_name \
     ON metric_samples (name, run_id, step, labels, ts_utc)";

/// One invocation's lines, and retention's "the lines of these".
pub const LOG_BY_INVOCATION: &str =
    "CREATE INDEX IF NOT EXISTS log_by_invocation ON log (invocation_id, seq)";

/// For retention, which removes an invocation's processes with it.
pub const PROCESSES_BY_INVOCATION: &str =
    "CREATE INDEX IF NOT EXISTS processes_by_invocation ON processes (invocation_id)";

/// For retention by age, and the newest invocations.
pub const INVOCATIONS_BY_START: &str = "CREATE INDEX IF NOT EXISTS invocations_by_start \
     ON invocations (started_at_utc, invocation_id)";

/// For each step's newest invocation, what the Manage screen draws.
pub const INVOCATIONS_BY_STEP: &str = "CREATE INDEX IF NOT EXISTS invocations_by_step \
     ON invocations (step, started_at_utc, invocation_id)";

/// For a request's invocations: its key leads with the invocation.
pub const REQUESTS_BY_REQUEST: &str = "CREATE INDEX IF NOT EXISTS invocation_requests_by_request \
     ON invocation_requests (request_id, invocation_id)";

/// For Manage's reads of a few metrics on every refresh, as
/// `metrics_by_name` is for an older build's runs.
pub const INVOCATION_METRICS_BY_NAME: &str = "CREATE INDEX IF NOT EXISTS \
     invocation_metrics_by_name ON invocation_metrics (name, step, labels)";
pub const INVOCATION_SAMPLES_BY_NAME: &str = "CREATE INDEX IF NOT EXISTS \
     invocation_samples_by_name ON invocation_metric_samples (name, step, labels, ts_utc)";
