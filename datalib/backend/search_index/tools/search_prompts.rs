//! The fixture's chunk prompts, one per line as `<hash>\t<prompt as JSON>`,
//! sorted: what `:ingested_tng_vectors` embeds. Read from a root whose
//! `keyword_index` steps have run, as the `embed` step reads them, and
//! each checked against its hash, so a prompt built differently from the
//! step's fails here rather than caching a vector under the wrong text.
//!
//! Usage: search_prompts <root> <out>

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_search_index::{prompt, Index};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, out] = args.as_slice() else {
        bail!("usage: search_prompts <root> <out>");
    };
    let root = PathBuf::from(root);
    let index = Index::open(&datalib_runtime::layout::grid_index_db(&root)).await?;
    let mut prompts: BTreeMap<String, String> = BTreeMap::new();
    let sources: Vec<String> = sources(&root)?;
    for source in &sources {
        let spans = index.chunk_spans(source).await?;
        if spans.is_empty() {
            continue;
        }
        let rows = {
            let (root, source) = (root.clone(), source.clone());
            tokio::task::spawn_blocking(move || rows(&root, &source)).await??
        };
        for span in spans {
            let (title, text) = rows
                .get(&span.markdown_uuid)
                .with_context(|| format!("{source}: no document {}", span.markdown_uuid))?;
            let piece = text
                .get(span.start..span.end)
                .with_context(|| format!("{}: a chunk past its text", span.markdown_uuid))?;
            let p = prompt::document(piece, Some(title));
            if blake3::hash(p.as_bytes()).to_hex().as_str() != span.hash {
                bail!(
                    "{}: a chunk whose prompt is not its hash",
                    span.markdown_uuid
                );
            }
            prompts.insert(span.hash, p);
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

/// Each document's title and text, by id.
fn rows(root: &Path, source: &str) -> Result<BTreeMap<String, (String, String)>> {
    let render_root = datalib_etl::layout::render_markdown_root(root, source);
    let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, None)? else {
        return Ok(BTreeMap::new());
    };
    let rows = store.markdown_rows();
    store.close();
    let mut out = BTreeMap::new();
    for row in rows? {
        let Some(md) = row.md_path.as_deref() else {
            continue;
        };
        let text = std::fs::read_to_string(root.join(md)).with_context(|| format!("read {md}"))?;
        out.insert(row.markdown_uuid, (row.title.unwrap_or_default(), text));
    }
    Ok(out)
}
