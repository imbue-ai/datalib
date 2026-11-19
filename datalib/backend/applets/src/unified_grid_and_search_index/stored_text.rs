//! A document's markdown as its source's render store spells it, and what
//! one item of it says, at the commit of that store the grid index read,
//! so the text matches the rows the grid shows: what the viewer and a
//! search's quotes read. Read-only: opening a document never writes.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_etl_render::section::{inside_box, SpelledSection};
use datalib_unified_grid_and_search_index::repo::DocumentStore;

/// One document spelled out, section by section in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spelled {
    pub sections: Vec<SpelledSection>,
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
            if let Some(sections) = store.document(document)? {
                out.insert(document.to_string(), Spelled { sections });
            }
            anyhow::Ok(())
        });
        store.close();
        read.with_context(|| format!("read documents from {source_id}'s render store"))?;
    }
    Ok(out)
}

/// What each item `wanted` names says, by its uuid: its body, or its piece
/// whole without the box it is drawn in. `wanted` is each item with the
/// document it is in; each source's store is opened once, at the commit
/// the grid index read. An item its store does not hold is left out.
pub fn item_texts(
    root: &Path,
    where_kept: &HashMap<String, DocumentStore>,
    wanted: &[(String, String)],
) -> Result<HashMap<String, String>> {
    type Asked = (Vec<String>, Vec<String>);
    let mut by_store: BTreeMap<(&str, &str), Asked> = BTreeMap::new();
    for (document, item) in wanted {
        let Some(kept) = where_kept.get(document) else {
            continue;
        };
        let asked = by_store
            .entry((kept.source_id.as_str(), kept.commit.as_str()))
            .or_default();
        asked.0.push(document.clone());
        asked.1.push(item.clone());
    }
    let mut out = HashMap::with_capacity(wanted.len());
    for ((source_id, commit), (documents, items)) in by_store {
        let rendered = datalib_etl::layout::render_markdown_root(root, source_id);
        let Some(store) = IndexedMarkdownStore::open_for_reading(&rendered, Some(commit))
            .with_context(|| format!("open {source_id}'s render store at {commit}"))?
        else {
            continue;
        };
        let read = store.item_texts(&documents, &items);
        store.close();
        for (uuid, text, class) in
            read.with_context(|| format!("read items from {source_id}'s render store"))?
        {
            let said = class
                .as_deref()
                .and_then(|class| inside_box(&uuid, class, &text))
                .map_or_else(|| text.clone(), str::to_string);
            out.insert(uuid, said);
        }
    }
    Ok(out)
}
