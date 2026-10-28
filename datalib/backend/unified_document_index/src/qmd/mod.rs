//! Bridge to qmd: its index read directly (keywords, vectors, state) and
//! the hit-to-row mapping.

pub mod index_state;
pub mod lex;
pub mod mapping;
pub mod snippet;
pub mod vectors;

pub use index_state::{DocIndexState, HeldIndex, OpenIndex, QmdIndexReader, QmdIndexSummary};
pub use mapping::{CollectionScope, GridIndex, GridRowRef, QmdHit};
pub use snippet::display_snippet;

/// The qmd version pin and the `Command` builder that spawns it live in
/// `datalib_runtime`, a crate with no dependencies. Re-exported here so
/// every `datalib_unified_document_index::qmd::…` call site uses
/// the one definition of each.
pub use datalib_runtime::qmd::{
    qmd_cache_home, qmd_command, qmd_index_path, qmd_state_dir, DEFAULT_QMD_VERSION, QMD_INDEX_REL,
};
