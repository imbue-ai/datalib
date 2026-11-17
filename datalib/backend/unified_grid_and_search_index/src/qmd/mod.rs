//! The search's hits, how they land on grid rows, and the query syntax and
//! arithmetic search by words and by meaning share.

pub mod index_state;
pub mod lex;
pub mod mapping;
pub mod snippet;
pub mod vectors;

pub use index_state::QmdIndexSummary;
pub use mapping::{CollectionScope, GridIndex, GridRowRef, QmdHit};
pub use snippet::display_snippet;
