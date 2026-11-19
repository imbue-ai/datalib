//! The fixture's chunks, cut as each source's `embed` cuts them: every
//! document the keyword index of a root holds, drawn by its render store,
//! cut by the model's tokenizer and filed into a new inference cache at
//! `<chunks out>`, which holds no vectors yet; and each chunk's prompt,
//! one per line as `<hash>\t<prompt as JSON>`, sorted, which
//! `:ingested_tng_vectors` embeds. A document its store draws from
//! another rendering than the index holds fails here, rather than filing
//! chunks no search would read.
//!
//! Usage: search_prompts <root> <model.gguf> <prompts out> <chunks out>

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_search_index::{ChunkedDoc, InferenceCache, Reader};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, model, prompts_out, chunks_out] = args.as_slice() else {
        bail!("usage: search_prompts <root> <model.gguf> <prompts out> <chunks out>");
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
    let cache = InferenceCache::open(Path::new(chunks_out)).await?;
    let mut prompts: BTreeMap<String, String> = BTreeMap::new();
    for source in &sources(&root)? {
        let held = index.documents_of(source).await?;
        if held.is_empty() {
            continue;
        }
        let docs = {
            let (root, source) = (root.clone(), source.clone());
            let uuids: Vec<String> = held.keys().cloned().collect();
            tokio::task::spawn_blocking(move || docs(&root, &source, uuids)).await??
        };
        let mut cut = Vec::with_capacity(docs.len());
        for doc in &docs {
            if held.get(&doc.markdown_uuid) != Some(&doc.content_hash) {
                bail!(
                    "{}: drawn from another rendering than the index holds",
                    doc.markdown_uuid
                );
            }
            let prompted = doc.prompted(&ends)?;
            cut.push(ChunkedDoc::from_prompted(doc, &prompted));
            for p in prompted {
                prompts.insert(p.hash, p.prompt);
            }
        }
        if cut.len() != held.len() {
            bail!(
                "{source}: the index holds {} documents and the store draws {}",
                held.len(),
                cut.len()
            );
        }
        cache.file_chunks(&cut).await?;
    }
    cache.close().await;
    index.close().await;
    let mut body = String::new();
    for (hash, p) in &prompts {
        body.push_str(&format!("{hash}\t{}\n", serde_json::to_string(p)?));
    }
    std::fs::write(prompts_out, body).with_context(|| format!("write {prompts_out}"))?;
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
    uuids: Vec<String>,
) -> Result<Vec<datalib_search_index::IndexDoc>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, source);
    let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
        return Ok(Vec::new());
    };
    let out = store.index_docs(source, &uuids);
    store.close();
    out
}
