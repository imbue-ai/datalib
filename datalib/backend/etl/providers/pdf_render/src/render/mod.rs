//! The `pdf` render side: convert each identified document to markdown
//! and emit it with its `grid_rows`.

pub mod convert;
pub mod grid_rows;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Buckets, Input, RawRange};
use datalib_etl_render::section::{msg_div_open, MSG_DIV_CLOSE};

pub use convert::RENDER_VERSION;
use datalib_etl_pdf::ingest::{RawDb, RenderTarget};

fn md_path_for(out_dir: &Path, blake3: &str) -> PathBuf {
    out_dir.join("docs").join(format!("{blake3}.md"))
}

pub fn doc_qmd_path_rel(stanza: &str, blake3: &str) -> String {
    format!(
        "{stanza}/{}/docs/{blake3}.md",
        datalib_etl::layout::RENDER_MARKDOWN_DIR
    )
}

pub struct RenderSummary {
    pub converted: usize,
    pub failed: usize,
    /// The documents whose conversion failed, by blake3. Their pages are
    /// stale rather than gone, so the processor leaves them undeclared.
    pub failed_blake3s: std::collections::HashSet<String>,
}

/// Load the work list. Split from [`render_targets`] so the async
/// database work finishes before the non-`Send` document sink enters
/// scope — otherwise the whole render future is non-`Send` and cannot
/// be driven by the `#[async_trait]` processor.
/// **`None` means the store could not be read**, which is not the same as
/// a corpus with nothing in it.
pub async fn load_targets(raw_dir: &Path) -> Result<Option<Vec<RenderTarget>>> {
    Ok(load(raw_dir, RawRange::cold()).await?.map(|l| l.targets))
}

/// The corpus at one commit, with what the diff since the cursor named.
pub struct Loaded {
    pub targets: Vec<RenderTarget>,
    /// The `pdf_scan_meta` row every document's absolute path comes from.
    pub scan_meta_id: Option<String>,
    pub scan: PdfScan,
}

/// One open of the store for the whole pass: the documents to convert
/// and the diff, at one commit — the driver's when it pinned one.
/// `None` means the store could not be read, which is not the same as
/// a corpus with nothing in it.
pub async fn load(raw_dir: &Path, range: RawRange<'_>) -> Result<Option<Loaded>> {
    let db_path = datalib_etl_pdf::ingest::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    let Some(db) = RawDb::open_reader_at(&db_path, range.pin).await? else {
        return Ok(None);
    };
    let loaded = async {
        let (scan_meta_id, targets) = match db.scan_root().await? {
            Some((id, root)) => (Some(id), db.convertible_documents(&root).await?),
            // No scan has run against this store yet.
            None => (None, Vec::new()),
        };
        let scan = scan_changed(&db, range, &targets).await?;
        Ok::<_, anyhow::Error>(Loaded {
            targets,
            scan_meta_id,
            scan,
        })
    }
    .await;
    // Closed before returning, on the error path too: the next open of
    // this store is a second connection until this one is gone.
    db.close().await;
    loaded.map(Some)
}

/// What a document reads: its own row, every path that holds its bytes,
/// and the scan row its absolute path is rooted at.
pub fn inputs_of(t: &RenderTarget, scan_meta_id: Option<&str>) -> Vec<Input> {
    let mut inputs = vec![Input::new("pdf_documents", &t.blake3)];
    inputs.extend(t.path_ids.iter().map(|id| Input::new("pdf_paths", id)));
    if let Some(id) = scan_meta_id {
        inputs.push(Input::new("pdf_scan_meta", id));
    }
    inputs
}

/// Every bucket a pass over `to_render` produces, for the processor to
/// declare: the ones looked at, with nothing, then the ones converted,
/// with what they read.
pub fn buckets_of(
    looked_at: Option<&std::collections::HashSet<String>>,
    converted: &[RenderTarget],
    scan_meta_id: Option<&str>,
) -> Buckets {
    looked_at
        .into_iter()
        .flatten()
        .map(|blake3| Bucket {
            key: grid_rows::document_uuid(blake3),
            inputs: Vec::new(),
        })
        .chain(converted.iter().map(|t| Bucket {
            key: grid_rows::document_uuid(&t.blake3),
            inputs: inputs_of(t, scan_meta_id),
        }))
        .collect()
}

pub fn render_targets(
    targets: &[RenderTarget],
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        converted: 0,
        failed: 0,
        failed_blake3s: Default::default(),
    };
    if targets.is_empty() {
        return Ok(summary);
    }
    progress.set_length(Some(targets.len() as u64));
    fs::create_dir_all(out_dir.join("docs"))
        .with_context(|| format!("create {}", out_dir.join("docs").display()))?;

    for t in targets {
        progress.inc(1);
        let md_path = md_path_for(out_dir, &t.blake3);
        let doc_uuid = grid_rows::document_uuid(&t.blake3);
        match render_one(t, &md_path, source_id, &doc_uuid) {
            Ok(rendered) => {
                summary.converted += 1;
                on_doc_complete(rendered)?;
            }
            Err(e) => {
                // One malformed document must not abort a corpus scan.
                summary.failed += 1;
                summary.failed_blake3s.insert(t.blake3.clone());
                tracing::warn!(
                    path = %t.rel_path, blake3 = %t.blake3, error = %e,
                    "pdf_render_failed"
                );
            }
        }
    }
    Ok(summary)
}

/// Convenience wrapper: load then render. Used by tests and by any
/// caller that does not need the two phases apart.
pub async fn render(
    raw_dir: &Path,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    // An unreadable store renders nothing. This wrapper does no deleting,
    // so unlike the processor's path it can treat the two alike.
    let targets = load_targets(raw_dir).await?.unwrap_or_default();
    render_targets(&targets, out_dir, source_id, progress, on_doc_complete)
}

fn render_one(
    t: &RenderTarget,
    md_path: &Path,
    source_id: &str,
    doc_uuid: &str,
) -> Result<RenderedMarkdown> {
    let pages = convert::convert(&t.abs_path)?;
    let title = grid_rows::display_title(t.title.as_deref(), &t.rel_path);

    let qmd_rel = doc_qmd_path_rel(source_id, &t.blake3);

    let mut body = String::new();
    body.push_str("---\n");
    body.push_str(&format!("provider: {}\n", grid_rows::PROVIDER));
    body.push_str(&format!("blake3: {}\n", yaml_str(&t.blake3)));
    body.push_str(&format!("title: {}\n", yaml_str(&title)));
    if let Some(a) = &t.author {
        body.push_str(&format!("author: {}\n", yaml_str(a)));
    }
    body.push_str(&format!("page_count: {}\n", t.page_count));
    body.push_str(&format!("pdf_type: {}\n", yaml_str(&t.pdf_type)));
    body.push_str(&format!("source_path: {}\n", yaml_str(&t.rel_path)));
    if t.copy_count > 1 {
        body.push_str(&format!("copies: {}\n", t.copy_count));
    }
    if let Some(c) = &t.doc_created_at {
        body.push_str(&format!("created_at: {}\n", yaml_str(c)));
    }
    if let Some(m) = &t.doc_modified_at {
        body.push_str(&format!("modified_at: {}\n", yaml_str(m)));
    }
    body.push_str("---\n\n");
    body.push_str(&format!("# {title}\n\n"));

    let mut page_rows: Vec<(u32, String)> = Vec::with_capacity(pages.len());
    for p in &pages {
        if p.non_textual {
            // A page we could not read. It goes in the markdown so the
            // gap is visible to whoever opens the document, but carries
            // no section wrapper and no grid row: there is no content to
            // navigate to, and the note is the same sentence on every
            // such page in every document. See `convert::note_for_page`.
            body.push_str(&p.text);
            body.push_str("\n\n");
            continue;
        }
        // Per-page section wrapper. The `data-section-uuid` must be
        // byte-equal to the page grid row's `uuid` or row→preview
        // navigation silently fails (see `etl::section` docs).
        let uuid = grid_rows::page_uuid(&t.blake3, p.number);
        body.push_str(&msg_div_open(&uuid, grid_rows::PROVIDER));
        body.push('\n');
        body.push_str(&p.text);
        body.push('\n');
        body.push_str(MSG_DIV_CLOSE);
        body.push_str("\n\n");
        page_rows.push((p.number, p.text.clone()));
    }

    if let Some(parent) = md_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(md_path, &body).with_context(|| format!("write {}", md_path.display()))?;

    let meta = grid_rows::DocumentMeta {
        blake3: &t.blake3,
        abs_path: &t.abs_path,
        title: t.title.as_deref(),
        author: t.author.as_deref(),
        rel_path: &t.rel_path,
        copy_count: t.copy_count,
        created_at: t.doc_created_at.as_deref(),
        modified_at: t.doc_modified_at.as_deref(),
        qmd_path: Some(&qmd_rel),
    };
    let rows = grid_rows::rows_for_document(&meta, &page_rows);

    Ok(RenderedMarkdown {
        markdown_uuid: doc_uuid.to_string(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        bucket_key: Some(doc_uuid.to_string()),
        md_path: md_path.to_path_buf(),
        render_version: RENDER_VERSION,
        rows,
        sections: Vec::new(),
        edges: Vec::new(),
        problems: Vec::new(),
    })
}

fn yaml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_path_is_content_named() {
        let p = md_path_for(Path::new("/out"), "abc123");
        assert_eq!(p, Path::new("/out/docs/abc123.md"));
    }

    #[test]
    fn qmd_path_is_data_root_relative_not_out_dir_relative() {
        // The bug this pins: `docs/abc123.md` (out-dir-relative) can
        // never match a qmd hit path, which is data-root-rooted.
        let rel = doc_qmd_path_rel("tng_pdfs", "abc123");
        assert_eq!(rel, "tng_pdfs/render_markdown/docs/abc123.md");
        assert!(!rel.starts_with("docs/"), "{rel}");
    }

    #[test]
    fn qmd_path_is_the_md_path_with_the_data_root_stripped() {
        // The invariant `markdowns.md_path` and `grid_rows.qmd_path`
        // must satisfy: same file, same spelling. `apply_one` derives
        // md_path's stored form by stripping the data root off the
        // absolute path, so building the two independently here and
        // comparing is what keeps them from drifting apart again.
        let root = Path::new("/data");
        let out_dir = datalib_etl::layout::render_markdown_root(root, "tng_pdfs");
        let md_path = md_path_for(&out_dir, "abc123");
        let from_md_path = md_path.strip_prefix(root).unwrap().to_string_lossy();
        assert_eq!(from_md_path, doc_qmd_path_rel("tng_pdfs", "abc123"));
    }

    #[test]
    fn yaml_quoting_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_str(r#"a"b"#), r#""a\"b""#);
        assert_eq!(yaml_str(r"a\b"), r#""a\\b""#);
    }
}

/// What the `dolt_diff` scan found, for a caller that will narrow its
/// target list with it.
#[derive(Debug, Clone, Default)]
pub struct PdfScan {
    /// `Some(set)` → convert only documents whose blake3 is in it: the
    /// ones the driver found stale through their declared inputs, plus
    /// the ones the diff named. `None` → convert everything the corpus
    /// holds.
    pub render: Option<std::collections::HashSet<String>>,
    /// Bucket keys the driver found stale that name no document the diff
    /// knows — declared with nothing so their pages go.
    pub gone: Vec<String>,
    pub new_head: Option<String>,
    pub elapsed: Option<std::time::Duration>,
}

/// Ask the raw store which documents moved since the cursor.
///
/// The bucket is the document's blake3, which is also its identity — for
/// this provider "changed" and "different document" are the same
/// statement. `pdf_paths` joins the union because a file appearing at a
/// new path is how a document enters the corpus, even when its bytes
/// were already known. A `pdf_scan_meta` change reaches every document
/// through the row each declared.
async fn scan_changed(
    db: &RawDb,
    range: RawRange<'_>,
    targets: &[RenderTarget],
) -> Result<PdfScan> {
    let pin = db
        .pin()
        .expect("open_reader returns a pinned handle")
        .clone();
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        db.pool(),
        range.cursor,
        &pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT blake3 FROM (
                    SELECT coalesce(to_blake3, from_blake3) AS blake3
                      FROM dolt_diff_pdf_documents
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_blake3, from_blake3)
                      FROM dolt_diff_pdf_paths
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE blake3 IS NOT NULL
            ",
        },
    )
    .await?;
    // The driver names stale buckets by document uuid; the diff by blake3.
    let by_uuid: std::collections::HashMap<String, &str> = targets
        .iter()
        .map(|t| (grid_rows::document_uuid(&t.blake3), t.blake3.as_str()))
        .collect();
    let narrowed = range.narrow_by(scan.render.as_ref(), |key| {
        by_uuid.get(key).map(|b| b.to_string())
    });
    Ok(PdfScan {
        render: narrowed.render,
        gone: narrowed.gone,
        new_head: scan.new_head,
        elapsed: scan.scan_elapsed,
    })
}
