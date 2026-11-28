//! `datalib-step markdown`: each document's markdown as its source's render
//! store spells it (`IndexedMarkdownStore::document`), byte for byte the
//! `.md` file render writes. Written to a folder, one file per document, for
//! an export or another tool; or one document printed. Read-only on the
//! data root.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_obs::status_line;

pub struct Ask<'a> {
    pub source: Option<&'a str>,
    pub document: Option<&'a str>,
    pub at: Option<&'a str>,
    /// Write each document to `<out>/<source>/<id>.md`; without it, the
    /// documents are printed.
    pub out: Option<&'a Path>,
}

pub fn run_cli(root: &Path, ask: &Ask<'_>) -> i32 {
    let stdout = std::io::stdout();
    match write(root, ask, &mut stdout.lock()) {
        Ok(written) => {
            if let Some(out) = ask.out {
                status_line!("wrote {written} documents under {}", out.display());
            }
            0
        }
        Err(e) => {
            status_line!("error: markdown: {e:#}");
            1
        }
    }
}

/// Where a document is written under `out`.
pub fn file_for(out: &Path, source: &str, markdown_uuid: &str) -> PathBuf {
    out.join(source).join(format!("{markdown_uuid}.md"))
}

/// Writes the documents `ask` names, to its folder or to `printed`, and
/// returns how many. One document asked for and not found is an error, so
/// a script that names one learns it is gone.
pub fn write(root: &Path, ask: &Ask<'_>, printed: &mut impl Write) -> Result<usize> {
    let sources = match ask.source {
        Some(s) => vec![s.to_string()],
        None => datalib_etl_render::grid_index::discover_sources(root),
    };
    let mut written = 0;
    for source in &sources {
        let render_root = datalib_etl::layout::render_markdown_root(root, source);
        let Some(store) = IndexedMarkdownStore::open_for_reading(&render_root, ask.at)
            .with_context(|| format!("open {source}'s render store"))?
        else {
            continue;
        };
        let done = write_source(&store, source, ask, printed);
        store.close();
        written += done?;
    }
    if let (Some(document), 0) = (ask.document, written) {
        anyhow::bail!("no source's render store holds document {document}");
    }
    Ok(written)
}

fn write_source(
    store: &IndexedMarkdownStore,
    source: &str,
    ask: &Ask<'_>,
    printed: &mut impl Write,
) -> Result<usize> {
    let uuids: Vec<String> = match ask.document {
        Some(d) => vec![d.to_string()],
        None => store
            .markdown_rows()?
            .into_iter()
            .map(|r| r.markdown_uuid)
            .collect(),
    };
    let mut written = 0;
    for uuid in uuids {
        let Some(text) = store.document_text(&uuid)? else {
            continue;
        };
        match ask.out {
            Some(out) => {
                let file = file_for(out, source, &uuid);
                std::fs::create_dir_all(file.parent().expect("a file under a folder"))?;
                std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
            }
            None if ask.document.is_some() => printed.write_all(text.as_bytes())?,
            None => {
                writeln!(printed, "=== {source} {uuid}")?;
                printed.write_all(text.as_bytes())?;
            }
        }
        written += 1;
    }
    Ok(written)
}
