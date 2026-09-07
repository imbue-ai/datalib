//! The progress bus: a plain-SQLite file any process can watch.

pub mod bus;

pub use bus::{open_or_create, snapshot, ProgressWriter, Snapshot};

use std::path::{Path, PathBuf};

/// Where the bus lives under a data root.
pub const PROGRESS_REL_PATH: &str = "system/progress.sqlite";

pub fn progress_path(data_root: &Path) -> PathBuf {
    data_root.join(PROGRESS_REL_PATH)
}

/// The two states the bus itself names. Every other value of
/// [`ProgressRow::state`] is a terminal status minted by whoever writes
/// the bus — the DAG runner's `RunState`, today — which this crate
/// deliberately does not enumerate: it is a leaf with no datalib
/// dependencies, and the scheduler's vocabulary is not its business.
/// "Not one of these two" is the whole of what the bus needs to know.
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

/// Whether a [`ProgressRow::state`] means the step is finished.
pub fn is_terminal(state: &str) -> bool {
    LiveState::parse(state).is_none()
}

/// One step's live state, as a reader sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressRow {
    pub step: String,
    /// A [`LiveState`], or the terminal status the scheduler gave it.
    pub state: String,
    /// Work units done, when the step reports any.
    pub done: Option<i64>,
    /// Total expected. `None` is *indeterminate*, not zero — a step that
    /// cannot know its total up front (a paginated API walk) says so,
    /// and a bar drawn from it should be a spinner.
    pub total: Option<i64>,
    /// The step's own words: "conversations.list", "3 of 9 channels".
    pub msg: Option<String>,
    pub updated_at: String,
}

/// The schema. `IF NOT EXISTS` throughout so opening an existing bus is
/// the same code path as making one.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS step_progress (
    step       TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL,
    state      TEXT NOT NULL,
    done       INTEGER,
    total      INTEGER,
    msg        TEXT,
    updated_at TEXT NOT NULL
)";
