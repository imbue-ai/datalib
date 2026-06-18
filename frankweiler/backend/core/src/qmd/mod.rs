//! Bridge to the `qmd` search CLI.
//!
//! `runner` shells out to the `qmd` CLI via `npx -y @tobilu/qmd@<version>`
//! and parses its JSON output into `QmdHit`s. `mapping` resolves those hits
//! to `grid_rows` UUIDs: it locates the hit's document by `qmd_path` (after
//! qmd's lowercase + `[_-]+ → -` normalization), then reads the rendered
//! markdown and maps the hit's matched line to the enclosing
//! `data-section-uuid`, falling back to the whole document when the line
//! can't be pinned.
//!
//! The data root is the same `<frankweiler_root>` everything else lives
//! under. qmd writes its index under `$XDG_CACHE_HOME/qmd/index.sqlite`,
//! so we point `XDG_CACHE_HOME` directly at the data root and the index
//! lands at `<root>/qmd/index.sqlite` alongside `rendered_md/` and
//! `backend_index.doltlite_db`.

pub mod daemon;
pub mod mapping;
pub mod runner;

pub use daemon::{QmdDaemon, QmdDaemonConfig};
pub use mapping::{GridIndex, GridRowRef, QmdHit, QueryMode};
pub use runner::{QmdRunner, QmdRunnerConfig, DEFAULT_COLLECTION, DEFAULT_QMD_VERSION};

use std::path::{Path, PathBuf};

/// Canonical sub-path of the qmd index, relative to `<root>`. qmd writes
/// here when invoked with `XDG_CACHE_HOME=<root>`.
pub const QMD_INDEX_REL: &str = "qmd/index.sqlite";

/// Resolve the qmd index file path under a data root.
pub fn qmd_index_path(root: &Path) -> PathBuf {
    root.join(QMD_INDEX_REL)
}

/// Resolve the XDG_CACHE_HOME the qmd CLI should run with for a data root.
/// This is the data root itself — qmd will write `qmd/index.sqlite` under it.
pub fn qmd_cache_home(root: &Path) -> PathBuf {
    root.to_path_buf()
}
