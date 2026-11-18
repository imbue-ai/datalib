//! A document's markdown as its source's render store spells it, at the
//! commit of that store the grid index read, so the text matches the rows
//! the grid shows. What the viewer and a search's quotes read in place of
//! the `.md` file render also writes. Read-only: opening a document never
//! writes.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_etl_render::section::SpelledPiece;
use datalib_schema::markdowns::SectionSpan;
use datalib_unified_grid_and_search_index::repo::DocumentStore;

/// One document spelled out: its text, and where each piece of it lies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spelled {
    pub text: String,
    pub layout: Vec<SectionSpan>,
}

impl Spelled {
    fn of(pieces: Vec<SpelledPiece>) -> Self {
        let layout = pieces
            .iter()
            .map(|p| SectionSpan {
                uuid: p.uuid.clone(),
                bytes: p.md.len(),
            })
            .collect();
        let text = pieces.into_iter().map(|p| p.md).collect();
        Self { text, layout }
    }
}

/// Each document `where_kept` names, spelled from its store, each source's
/// store opened once at the commit the grid index read. A document its
/// store no longer holds there is left out; a store that will not open is
/// an error for the whole batch, since every document of it would be.
pub fn spell(
    root: &Path,
    where_kept: &HashMap<String, DocumentStore>,
) -> Result<HashMap<String, Spelled>> {
    let mut by_store: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for (document, kept) in where_kept {
        by_store
            .entry((kept.source_id.as_str(), kept.commit.as_str()))
            .or_default()
            .push(document.as_str());
    }
    let mut out = HashMap::with_capacity(where_kept.len());
    for ((source_id, commit), documents) in by_store {
        let rendered = datalib_etl::layout::render_markdown_root(root, source_id);
        let Some(store) = IndexedMarkdownStore::open_for_reading(&rendered, Some(commit))
            .with_context(|| format!("open {source_id}'s render store at {commit}"))?
        else {
            continue;
        };
        let read = documents.iter().try_for_each(|&document| {
            if let Some(pieces) = store.document(document)? {
                out.insert(document.to_string(), Spelled::of(pieces));
            }
            anyhow::Ok(())
        });
        store.close();
        read.with_context(|| format!("read documents from {source_id}'s render store"))?;
    }
    Ok(out)
}
