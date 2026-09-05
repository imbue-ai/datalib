//! The progress bus: a plain-SQLite file any process can watch.

pub mod bus;

pub use bus::{open_or_create, snapshot, ProgressWriter, Snapshot};

use std::path::{Path, PathBuf};

/// Where the bus lives under a data root.
pub const PROGRESS_REL_PATH: &str = "system/progress.sqlite";

pub fn progress_path(data_root: &Path) -> PathBuf {
    data_root.join(PROGRESS_REL_PATH)
}

/// One step's live state, as a reader sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressRow {
    pub step: String,
    /// `pending` | `running` | the terminal status the scheduler gave it.
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
