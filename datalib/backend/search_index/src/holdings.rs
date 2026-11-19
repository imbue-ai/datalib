//! What the index holds of each source: its documents, and how many of
//! them are not embedded yet. `unified_grid_and_search_index` reports it;
//! the live bake checks it.

use std::path::Path;

use anyhow::Result;
use datalib_runtime::layout;

use crate::{model_key, Reader};

/// What the search index holds of one source.
#[derive(Debug, PartialEq, Eq)]
pub struct Holding {
    pub source: String,
    pub documents: usize,
    /// Documents its `embed` has not cut into chunks from the rendering
    /// the index holds, or whose chunks lack a vector: what Meaning does
    /// not find yet. Every one, for a source with no `embed`.
    pub not_embedded: usize,
}

/// What the index holds of each of `groups`; nothing before the first
/// keyword pass made it.
pub async fn holdings(root: &Path, groups: &[String]) -> Result<Vec<Holding>> {
    let Some(reader) = Reader::open(
        &layout::grid_index_db(root),
        &layout::inference_cache_db(root),
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let out = holdings_in(&reader, groups).await;
    reader.close().await;
    out
}

async fn holdings_in(reader: &Reader, groups: &[String]) -> Result<Vec<Holding>> {
    let model = model_key();
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let (documents, embedded) = reader.coverage_of(&model, Some(group)).await?;
        out.push(Holding {
            source: group.clone(),
            documents: usize::try_from(documents)?,
            not_embedded: usize::try_from(documents - embedded)?,
        });
    }
    Ok(out)
}
