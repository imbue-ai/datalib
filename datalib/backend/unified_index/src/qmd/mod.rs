//! Bridge to qmd: the long-lived `qmd mcp` daemon the grid searches
//! through, the hit-to-row mapping, and the index's own state.

pub mod daemon;
pub mod index_state;
pub mod lex;
pub mod mapping;
pub mod snippet;
pub mod vectors;

pub use daemon::{QmdDaemon, QmdDaemonConfig};
pub use index_state::{DocIndexState, QmdIndexReader, QmdIndexSummary};
pub use mapping::{CollectionScope, GridIndex, GridRowRef, QmdHit, QueryMode};
pub use snippet::display_snippet;

/// The qmd version pin and the `Command` builder that spawns it moved
/// down into `datalib_runtime` — a crate with no dependencies — so that
/// `qmd_indexer_bin` can reach them without linking this crate. Bazel
/// keys the fixture's embedding action on that binary's digest, so
/// everything it links is a crate whose next edit costs a ~90s CPU-only
/// embed on CI. Re-exported here so the daemon and every
/// existing `datalib_unified_index::qmd::…` call site are unchanged, and
/// so there is still exactly one definition of each.
pub use datalib_runtime::qmd::{
    qmd_cache_home, qmd_command, qmd_index_path, qmd_state_dir, DEFAULT_QMD_VERSION, QMD_INDEX_REL,
};
