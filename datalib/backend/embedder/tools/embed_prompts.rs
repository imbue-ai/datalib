//! Embeds every prompt `search_prompts` wrote, one per line as
//! `<hash>\t<vector as JSON>`, for `cache_vectors` to put in an inference
//! cache. The fixture's slow action (`//tests/fixtures:ingested_tng_vectors`),
//! so it links the embedder and nothing else: an edit to the search index
//! does not make Bazel embed again.
//!
//! Usage: embed_prompts <prompts> <model.gguf> <vectors>

use std::path::Path;

use anyhow::{bail, Context, Result};
use datalib_embedder::{Device, Embedder};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, prompts, model, out] = args.as_slice() else {
        bail!("usage: embed_prompts <prompts> <model.gguf> <vectors>");
    };
    let text = std::fs::read_to_string(prompts).with_context(|| format!("read {prompts}"))?;
    let mut embedder = Embedder::open(Path::new(model), Device::Best, 0)?;
    let mut body = String::new();
    for line in text.lines() {
        let (hash, p) = line.split_once('\t').context("a line with no tab")?;
        let p: String = serde_json::from_str(p)?;
        let (vector, _) = embedder.embed_fitted(&p)?;
        body.push_str(&format!("{hash}\t{}\n", serde_json::to_string(&vector)?));
    }
    // On Metal, ggml aborts at exit while a model is still loaded.
    drop(embedder);
    std::fs::write(out, body).with_context(|| format!("write {out}"))
}
