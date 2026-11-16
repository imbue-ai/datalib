//! What the index holds of each source: its documents, and how many of
//! their chunks have no vector yet. The aggregator reports it; the live
//! bake checks it.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use datalib_runtime::layout;

use crate::{model_key, Index, InferenceCache};

/// What the search index holds of one source.
#[derive(Debug, PartialEq, Eq)]
pub struct Holding {
    pub source: String,
    pub documents: usize,
    /// Distinct chunks with no vector in the inference cache yet.
    pub not_embedded: usize,
}

/// What the index holds of each of `groups`; nothing before the first
/// keyword pass made it.
pub async fn holdings(root: &Path, groups: &[String]) -> Result<Vec<Holding>> {
    let index_path = layout::grid_index_db(root);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::open(&index_path).await?;
    let cache = match InferenceCache::open(&layout::inference_cache_db(root)).await {
        Ok(cache) => cache,
        Err(e) => {
            index.close().await;
            return Err(e);
        }
    };
    let out = holdings_in(&index, &cache, groups).await;
    cache.close().await;
    index.close().await;
    out
}

async fn holdings_in(
    index: &Index,
    cache: &InferenceCache,
    groups: &[String],
) -> Result<Vec<Holding>> {
    let model = model_key();
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let documents = index.indexed(group).await?.len();
        let hashes: Vec<String> = index
            .chunk_spans(group)
            .await?
            .into_iter()
            .map(|s| s.hash)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let not_embedded = cache.missing(&model, &hashes).await?.len();
        out.push(Holding {
            source: group.clone(),
            documents,
            not_embedded,
        });
    }
    Ok(out)
}
