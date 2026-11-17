//! The fixture's chunk prompts, one per line as `<hash>\t<prompt as JSON>`,
//! sorted: what `:ingested_tng_vectors` embeds. Read from a root whose
//! `grid_index` has run, drawn by the store as the `embed` step
//! draws them, and each one the index names found among them, so a prompt
//! drawn differently from the step's fails here rather than caching a
//! vector under the wrong text.
//!
//! Usage: search_prompts <root> <model.gguf> <out>

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_search_index::Reader;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, model, out] = args.as_slice() else {
        bail!("usage: search_prompts <root> <model.gguf> <out>");
    };
    let root = PathBuf::from(root);
    let tokenizer = datalib_embedder::Tokenizer::open(Path::new(model))?;
    let ends = |text: &str| tokenizer.ends(text);
    let Some(index) = Reader::open(
        &datalib_runtime::layout::grid_index_db(&root),
        &datalib_runtime::layout::inference_cache_db(&root),
    )
    .await?
    else {
        bail!("{}: no search index", root.display());
    };
    let mut prompts: BTreeMap<String, String> = BTreeMap::new();
    let sources: Vec<String> = sources(&root)?;
    for source in &sources {
        let spans = index.chunk_hashes(source).await?;
        if spans.is_empty() {
            continue;
        }
        let docs = {
            let (root, source) = (root.clone(), source.clone());
            let uuids: Vec<String> = spans.iter().map(|(doc, _)| doc.clone()).collect();
            tokio::task::spawn_blocking(move || docs(&root, &source, uuids)).await??
        };
        let mut drawn: BTreeMap<String, String> = BTreeMap::new();
        for doc in &docs {
            for p in doc.prompted(&ends)? {
                drawn.insert(p.hash, p.prompt);
            }
        }
        for (doc, hash) in spans {
            let p = drawn
                .get(&hash)
                .with_context(|| format!("{doc}: a chunk its document no longer draws"))?;
            prompts.insert(hash, p.clone());
        }
    }
    index.close().await;
    let mut body = String::new();
    for (hash, p) in &prompts {
        body.push_str(&format!("{hash}\t{}\n", serde_json::to_string(p)?));
    }
    std::fs::write(out, body).with_context(|| format!("write {out}"))?;
    Ok(())
}

/// Every group under the root with a render store.
fn sources(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if datalib_etl::layout::render_markdown_root(root, &name).is_dir() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// These documents as the embed step draws them.
fn docs(
    root: &Path,
    source: &str,
    mut uuids: Vec<String>,
) -> Result<Vec<datalib_search_index::IndexDoc>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, source);
    let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
        return Ok(Vec::new());
    };
    uuids.sort();
    uuids.dedup();
    let out = store.index_docs(source, &uuids);
    store.close();
    out
}
