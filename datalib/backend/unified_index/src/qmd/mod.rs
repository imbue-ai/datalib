//! Bridge to the `qmd` search CLI.
//!
//! `runner` shells out to the `qmd` CLI via [`qmd_command`] (the
//! app-bundled Node runtime when staged, else `npx -y
//! @tobilu/qmd@<version>`) and parses its JSON output into `QmdHit`s. `mapping` resolves those hits
//! to `grid_rows` UUIDs: it locates the hit's document by `qmd_path` (after
//! qmd's lowercase + `[_-]+ → -` normalization), then reads the rendered
//! markdown and maps the hit's matched line to the enclosing
//! `data-section-uuid`, falling back to the whole document when the line
//! can't be pinned.
//!
//! qmd writes its index under `$XDG_CACHE_HOME/qmd/index.sqlite`, so we point
//! `XDG_CACHE_HOME` at `<root>/system` and the index lands at
//! `<root>/unified_index/qmd/index.sqlite` alongside the grid index
//! (see [`datalib_core::layout`]). The *scan* root stays `<root>` so qmd still finds
//! every stanza's `rendered_md/`.

pub mod daemon;
pub mod index_state;
pub mod mapping;
pub mod runner;

pub use daemon::{QmdDaemon, QmdDaemonConfig};
pub use index_state::{DocIndexState, QmdIndexReader, QmdIndexSummary};
pub use mapping::{GridIndex, GridRowRef, QmdHit, QueryMode};
pub use runner::{QmdRunner, QmdRunnerConfig, DEFAULT_COLLECTION};

/// The qmd version pin and the `Command` builder that spawns it moved
/// down into `datalib_runtime` — a crate with no dependencies — so that
/// `qmd_indexer_bin` can reach them without linking this crate. Bazel
/// keys the fixture's embedding action on that binary's digest, so
/// everything it links is a crate whose next edit costs a ~90s CPU-only
/// embed on CI. Re-exported here so the runner, the daemon and every
/// existing `datalib_unified_index::qmd::…` call site are unchanged, and
/// so there is still exactly one definition of each.
pub use datalib_runtime::qmd::{
    qmd_cache_home, qmd_command, qmd_index_path, DEFAULT_QMD_VERSION, QMD_INDEX_REL,
};
