//! Puts the vectors `embed_prompts` wrote into an inference cache, under
//! the model the search index looks them up by: the fixture's
//! `inference_cache.sqlite`, rebuilt in seconds whenever the search index
//! changes, while the embedding itself stays cached.
//!
//! Usage: cache_vectors <vectors> <inference_cache.sqlite>

use std::path::Path;

use anyhow::{bail, Context, Result};
use datalib_search_index::InferenceCache;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, vectors, out] = args.as_slice() else {
        bail!("usage: cache_vectors <vectors> <inference_cache.sqlite>");
    };
    let text = std::fs::read_to_string(vectors).with_context(|| format!("read {vectors}"))?;
    let mut rows: Vec<(String, Vec<f32>)> = Vec::new();
    for line in text.lines() {
        let (hash, v) = line.split_once('\t').context("a line with no tab")?;
        rows.push((hash.to_string(), serde_json::from_str(v)?));
    }
    let cache = InferenceCache::open(Path::new(out)).await?;
    let put = cache.put(&datalib_search_index::model_key(), &rows).await;
    cache.close().await;
    put
}
