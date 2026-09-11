//! The run store: one plain-SQLite file per data root that the runner
//! writes and anything can read. Holds what every run did — each step's
//! state, its log lines, and its metrics — across runs, until retention
//! removes the old ones. A leaf crate with no datalib dependencies beyond
//! `datalib_time`, so the runner takes it without taking `datalib_core`.

pub mod store;

pub use store::{
    canonical_labels, log_after, open_or_create, runs, snapshot, snapshot_of, LogRow, MetricRow,
    RunRow, RunWriter, Snapshot, StepRow,
};

use std::path::{Path, PathBuf};

/// Where the store lives under a data root.
pub const RUNS_REL_PATH: &str = "system/runs.sqlite";

pub fn runs_path(data_root: &Path) -> PathBuf {
    data_root.join(RUNS_REL_PATH)
}

/// The two states the store itself names. Every other value of
/// [`StepRow::state`] is a terminal status minted by whoever writes the
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

/// Whether a [`StepRow::state`] means the step is finished.
pub fn is_terminal(state: &str) -> bool {
    LiveState::parse(state).is_none()
}

/// How much history to keep. Both limits apply; a run older than
/// `max_age_days` goes even when fewer than `max_runs` exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_runs: u32,
    pub max_age_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_runs: 100,
            max_age_days: 30,
        }
    }
}

/// Bumped whenever [`SCHEMA`] changes shape. A store carrying another
/// version is deleted and remade rather than migrated: nothing in it is
/// load-bearing, and a migration is code that would exist only to keep
/// old log lines.
pub const SCHEMA_VERSION: i32 = 2;

/// The schema. `IF NOT EXISTS` throughout so opening an existing store is
/// the same code path as making one. `log.seq` is the rowid, so a reader
/// tailing "everything after N" needs no timestamp arithmetic.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS runs (
    run_id      TEXT PRIMARY KEY,
    started_at  TEXT NOT NULL,
    finished_at TEXT
);
CREATE TABLE IF NOT EXISTS step_runs (
    run_id      TEXT NOT NULL,
    step        TEXT NOT NULL,
    state       TEXT NOT NULL,
    attempt     INTEGER NOT NULL DEFAULT 0,
    started_at  TEXT,
    finished_at TEXT,
    error       TEXT,
    msg         TEXT,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (run_id, step)
);
CREATE TABLE IF NOT EXISTS log (
    seq     INTEGER PRIMARY KEY,
    run_id  TEXT NOT NULL,
    step    TEXT,
    attempt INTEGER NOT NULL DEFAULT 0,
    ts      TEXT NOT NULL,
    stream  TEXT,
    level   TEXT NOT NULL,
    target  TEXT,
    thread  TEXT,
    msg     TEXT NOT NULL,
    fields  TEXT
);
CREATE INDEX IF NOT EXISTS log_by_run_step ON log (run_id, step, seq);
CREATE TABLE IF NOT EXISTS metrics (
    run_id     TEXT NOT NULL,
    step       TEXT NOT NULL,
    name       TEXT NOT NULL,
    labels     TEXT NOT NULL DEFAULT '',
    value      INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (run_id, step, name, labels)
);
CREATE TABLE IF NOT EXISTS metric_samples (
    run_id TEXT NOT NULL,
    step   TEXT NOT NULL,
    name   TEXT NOT NULL,
    labels TEXT NOT NULL DEFAULT '',
    ts     TEXT NOT NULL,
    value  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS metric_samples_by_series
    ON metric_samples (run_id, step, name, labels, ts);
";
