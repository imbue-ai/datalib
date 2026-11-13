//! Embeds every prompt `search_prompts` wrote into an inference cache, as
//! the `embed` step would: the fixture's vectors, cached by Bazel on the
//! prompts alone.
//!
//! Usage: embed_prompts <prompts> <model.gguf> <inference_cache.sqlite>

use std::path::Path;

use anyhow::{bail, Context, Result};
use datalib_embedder::{Device, Embedder};
use datalib_search_index::InferenceCache;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, prompts, model, out] = args.as_slice() else {
        bail!("usage: embed_prompts <prompts> <model.gguf> <inference_cache.sqlite>");
    };
    let text = std::fs::read_to_string(prompts).with_context(|| format!("read {prompts}"))?;
    let mut embedder = Embedder::open(Path::new(model), Device::Best, 0)?;
    let mut vectors = Vec::new();
    for line in text.lines() {
        let (hash, p) = line.split_once('\t').context("a line with no tab")?;
        let p: String = serde_json::from_str(p)?;
        let (vector, _) = embedder.embed_fitted(&p)?;
        vectors.push((hash.to_string(), vector));
    }
    // On Metal, ggml aborts at exit while a model is still loaded.
    drop(embedder);
    let cache = InferenceCache::open(Path::new(out)).await?;
    let put = cache
        .put(&datalib_search_index::model_key(), &vectors)
        .await;
    cache.close().await;
    put?;
    Ok(())
}
