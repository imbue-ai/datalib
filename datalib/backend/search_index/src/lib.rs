//! The search index: each document's messages in a keyword index, its
//! chunks' embeddings, and where each falls in the document, keyed by
//! `markdown_uuid` and holding none of the text. A document comes in as a
//! value, so whoever has it in memory can index it; nothing here reads a
//! file. The plan: docs/dev/plans/search_index_from_ram.md.

use std::borrow::Cow;

pub mod cache;
pub mod chunk;
mod file;
pub mod owed;
pub mod prompt;
mod store;

pub use cache::InferenceCache;
pub use owed::{owed, Owed};
pub use store::{ChunkSpan, Index};

/// A rendered document, its sections in order. Laid end to end, the
/// sections' texts are the document; every offset the index stores is a
/// byte offset into that.
#[derive(Debug, Clone)]
pub struct IndexDoc {
    pub source_id: String,
    pub markdown_uuid: String,
    /// What the render store records for this rendering of the document.
    pub content_hash: String,
    pub title: String,
    pub sections: Vec<IndexSection>,
}

#[derive(Debug, Clone)]
pub struct IndexSection {
    /// The message's `data-section-uuid`; `None` for text above the first
    /// message, or a document that has no messages.
    pub uuid: Option<String>,
    pub text: String,
}

impl IndexDoc {
    /// The sections, or one empty one, so the title is indexed for a
    /// document that has no text.
    fn sections_or_one(&self) -> Cow<'_, [IndexSection]> {
        if self.sections.is_empty() {
            Cow::Owned(vec![IndexSection {
                uuid: None,
                text: String::new(),
            }])
        } else {
            Cow::Borrowed(&self.sections)
        }
    }
}
