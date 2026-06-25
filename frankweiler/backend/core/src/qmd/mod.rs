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
//! qmd writes its index under `$XDG_CACHE_HOME/qmd/index.sqlite`, so we point
//! `XDG_CACHE_HOME` at `<root>/system` and the index lands at
//! `<root>/system/qmd/index.sqlite` alongside the other aggregate processors
//! (see [`crate::layout`]). The *scan* root stays `<root>` so qmd still finds
//! every stanza's `rendered_md/`.

pub mod daemon;
pub mod mapping;
pub mod runner;

pub use daemon::{QmdDaemon, QmdDaemonConfig};
pub use mapping::{GridIndex, GridRowRef, QmdHit, QueryMode};
pub use runner::{QmdRunner, QmdRunnerConfig, DEFAULT_COLLECTION, DEFAULT_QMD_VERSION};

use std::path::{Path, PathBuf};

/// Canonical sub-path of the qmd index, relative to `<root>`. qmd writes
/// here when invoked with `XDG_CACHE_HOME=<root>/system` (see
/// [`qmd_cache_home`]).
pub const QMD_INDEX_REL: &str = "system/qmd/index.sqlite";

/// Resolve the qmd index file path under a data root.
pub fn qmd_index_path(root: &Path) -> PathBuf {
    crate::layout::qmd_dir(root).join("index.sqlite")
}

/// Resolve the `XDG_CACHE_HOME` the qmd CLI should run with for a data root:
/// `<root>/system`, so qmd writes its `qmd/index.sqlite` under `system/`.
pub fn qmd_cache_home(root: &Path) -> PathBuf {
    crate::layout::system_dir(root)
}
