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

/// The qmd version pin and the `Command` builder that spawns it live in
/// `datalib_runtime`, a crate with no dependencies. Re-exported here so
/// the daemon and every `datalib_unified_index::qmd::…` call site use
/// the one definition of each.
pub use datalib_runtime::qmd::{
    qmd_cache_home, qmd_command, qmd_index_path, qmd_state_dir, DEFAULT_QMD_VERSION, QMD_INDEX_REL,
};
