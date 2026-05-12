//! Rust port of `src/qmd_bridge/` (Python).
//!
//! `runner` shells out to the `qmd` CLI via `npx -y @tobilu/qmd@<version>`
//! and parses its JSON output into `QmdHit`s. `mapping` resolves those hits
//! to `grid_rows` UUIDs (primary key: embedded `m-{uuid}` ids in the
//! snippet; fallback: row whose `qmd_path` matches the hit's file path
//! after qmd's lowercase + `[_-]+ → -` normalization).
//!
//! The data root is the same `<frankweiler_root>` everything else lives
//! under: `mirror.sqlite` next to the rendered markdown tree, with the qmd
//! index at `<root>/.frankweiler/qmd/index.sqlite` (qmd's natural cache
//! location when `XDG_CACHE_HOME=<root>/.frankweiler`).

pub mod daemon;
pub mod mapping;
pub mod runner;

pub use daemon::{QmdDaemon, QmdDaemonConfig};
pub use mapping::{GridIndex, GridRowRef, QmdHit, QueryMode};
pub use runner::{QmdRunner, QmdRunnerConfig, DEFAULT_COLLECTION, DEFAULT_QMD_VERSION};

use std::path::{Path, PathBuf};

/// Canonical sub-path of the qmd index, relative to `<root>`. qmd writes
/// here when invoked with `XDG_CACHE_HOME=<root>/.frankweiler`.
pub const QMD_INDEX_REL: &str = ".frankweiler/qmd/index.sqlite";

/// Canonical sub-path of the qmd XDG cache home, relative to `<root>`.
pub const QMD_CACHE_HOME_REL: &str = ".frankweiler";

/// Resolve the qmd index file path under a data root.
pub fn qmd_index_path(root: &Path) -> PathBuf {
    root.join(QMD_INDEX_REL)
}

/// Resolve the XDG_CACHE_HOME the qmd CLI should run with for a data root.
pub fn qmd_cache_home(root: &Path) -> PathBuf {
    root.join(QMD_CACHE_HOME_REL)
}
