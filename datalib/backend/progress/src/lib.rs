//! The progress bus: a plain-SQLite file any process can watch.
//!
//! # What it is for
//!
//! A sync's live state — which step is running, how far in, what it is
//! doing right now — used to exist only as NDJSON on the runner's
//! stderr. That has two properties we do not want:
//!
//!   * **one reader.** Whoever inherited the pipe, and nobody else. So
//!     a sync started from a terminal was invisible to the app, and a
//!     sync started by the app was invisible to a terminal.
//!   * **fragile.** A stray `println!` anywhere in a step corrupts the
//!     stream. The tree already carries a `status_line!` macro and
//!     clippy lints specifically to stop that happening.
//!
//! This is a file instead. One writer (the runner, which already holds
//! `system/runner-lock`), any number of readers, and readers that are
//! not us: `sqlite3`, a Python script, `watch` — anything with a SQLite
//! binding, because the file is ordinary SQLite.
//!
//! # Why ordinary SQLite is available at all
//!
//! Every SQLite handle in this tree is doltlite (`MODULE.bazel` routes
//! `libsqlite3-sys` at the doltlite archive). The reason a plain file
//! still works — and the reason this crate exists rather than a table in
//! one of the doltlite stores — is measured, not assumed:
//!
//! | | doltlite-format file (`CTLD`) | plain-SQLite file |
//! |---|---|---|
//! | `PRAGMA journal_mode` | `wal` | `delete`, and accepts `=WAL` |
//! | `.<name>-lock` sidecar | created | **not** created |
//! | 200 single-row updates | ~50 ms each (see `grid_index`) | 0.06 s total, ~0.3 ms each |
//! | reader in another process while writing | contends | 40/40 clean, including a wholly separate SQLite |
//!
//! On a plain file doltlite *is* stock SQLite. The per-commit cost that
//! makes the doltlite stores unsuitable for something written tens of
//! times a second is a property of the prolly-tree container, not of the
//! library.
//!
//! By default it will not *create* a plain file — given a missing or
//! zero-length path it claims it as `CTLD`. But it does not have to be
//! talked out of that by hand: doltlite has a URI parameter for exactly
//! this, `doltlite_engine=sqlite`, which selects the stock engine for a
//! new empty file and is ignored once the file has content. See
//! [`bus::open_or_create`] for the one line that uses it.
//!
//! # What it deliberately is not
//!
//! **Not durable, and not history.** The file is discarded and remade
//! at the start of every run. It answers "what is happening now"; what
//! *happened* is [`datalib_dag::state`] (per-step outcomes) and each
//! source's own `sync_runs` table (per-source provenance). Nothing here
//! is worth an fsync, which is why `synchronous = OFF` is correct rather
//! than reckless: a torn progress bar after a power cut costs nothing.
//!
//! **Not the log.** Progress is coalesced — only the newest tick per
//! step survives — so a reader polling at 2 Hz sees a smooth bar and a
//! reader polling at 0.1 Hz sees a coarse one, and neither sees a
//! backlog. The NDJSON stream remains the log, and remains what the job
//! log file captures.

pub mod bus;

pub use bus::{open_or_create, snapshot, ProgressWriter, Snapshot};

use std::path::{Path, PathBuf};

/// Where the bus lives under a data root.
pub const PROGRESS_REL_PATH: &str = "system/progress.sqlite";

pub fn progress_path(data_root: &Path) -> PathBuf {
    data_root.join(PROGRESS_REL_PATH)
}

/// One step's live state, as a reader sees it.
///
/// Named `ProgressRow` rather than `StepProgress` because
/// `datalib_dag::events::StepProgress` already exists and means
/// something different: the *handle a step emits through*. This is the
/// row that comes back out the other end.
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
