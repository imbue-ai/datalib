//! `datalib-step index-text`: what the search index draws of each document,
//! by the call the keyword and embed passes make
//! (`IndexedMarkdownStore::index_docs`), so what it prints is what is
//! indexed. Read-only; for a person, an agent, and the goldens that watch
//! the index text for drift.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_obs::status_line;
use datalib_search_index::chunk::TokenEnds;

/// Documents drawn together, as the keyword pass batches them.
const BATCH: usize = 200;

pub(crate) struct Ask<'a> {
    pub(crate) source: Option<&'a str>,
    pub(crate) document: Option<&'a str>,
    pub(crate) at: Option<&'a str>,
    pub(crate) chunks: bool,
}

pub(crate) fn run_cli(root: &Path, ask: &Ask<'_>) -> i32 {
    let stdout = std::io::stdout();
    let written = if ask.chunks {
        tokenizer(root).and_then(|t| {
            let ends = |text: &str| t.ends(text);
            write(root, ask, Some(&ends), &mut stdout.lock())
        })
    } else {
        write(root, ask, None, &mut stdout.lock())
    };
    match written {
        Ok(()) => 0,
        Err(e) => {
            status_line!("error: index-text: {e:#}");
            1
        }
    }
}

/// The model's tokenizer, which cuts the chunks, from where the steps keep
/// the model for this root. Read-only: it fetches nothing.
fn tokenizer(root: &Path) -> Result<datalib_embedder::Tokenizer> {
    let model = crate::search_index::model_file(&datalib_qmd_models::models_dir_for(root));
    anyhow::ensure!(
        model.is_file(),
        "{} is missing; `datalib-step pull-models` puts it in place",
        model.display()
    );
    datalib_embedder::Tokenizer::open(&model)
}

/// What the index draws of each document; with `--chunks`, its chunks as
/// `ends` (the model's tokenizer) cuts them.
pub(crate) fn write(
    root: &Path,
    ask: &Ask<'_>,
    ends: Option<TokenEnds<'_>>,
    out: &mut impl Write,
) -> Result<()> {
    let sources = match ask.source {
        Some(s) => vec![s.to_string()],
        None => sources(root)?,
    };
    for source in &sources {
        let render_root = datalib_etl::layout::render_markdown_root(root, source);
        let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, ask.at)
            .with_context(|| format!("open {source}'s render store"))?
        else {
            continue;
        };
        let written = write_source(&store, source, ask, ends, out);
        store.close();
        written?;
    }
    Ok(())
}

fn write_source(
    store: &IndexedMarkdownStore,
    source: &str,
    ask: &Ask<'_>,
    ends: Option<TokenEnds<'_>>,
    out: &mut impl Write,
) -> Result<()> {
    let uuids: Vec<String> = match ask.document {
        Some(d) => vec![d.to_string()],
        None => store
            .markdown_rows()?
            .into_iter()
            .filter(crate::search_index::searchable)
            .map(|r| r.markdown_uuid)
            .collect(),
    };
    for batch in uuids.chunks(BATCH) {
        for doc in store.index_docs(source, batch)? {
            writeln!(out, "=== {source} {} {}", doc.markdown_uuid, doc.title)?;
            if let Some(ends) = ends.filter(|_| ask.chunks) {
                for (n, p) in doc.prompted(ends)?.iter().enumerate() {
                    writeln!(out, "--- chunk {n} {}\n{}", p.hash, p.prompt)?;
                }
            } else {
                for s in &doc.sections {
                    writeln!(out, "--- {}", s.uuid.as_deref().unwrap_or("-"))?;
                    out.write_all(s.text.as_bytes())?;
                }
            }
        }
    }
    Ok(())
}

/// Every group under the root with a render store, in order.
fn sources(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let rendered = datalib_etl::layout::render_markdown_root(root, &name);
        if datalib_etl_render::indexed_markdown::path_for(&rendered).is_file() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}
