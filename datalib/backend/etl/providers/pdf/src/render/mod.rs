//! The `pdf` render side: convert each identified document to markdown
//! and emit it with its `grid_rows`.

pub mod convert;
pub mod grid_rows;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl::section::{msg_div_open, MSG_DIV_CLOSE};

use crate::download::{RawDb, RenderTarget};
pub use convert::RENDER_VERSION;

fn md_path_for(out_dir: &Path, blake3: &str) -> PathBuf {
    out_dir.join("docs").join(format!("{blake3}.md"))
}

pub fn doc_qmd_path_rel(stanza: &str, blake3: &str) -> String {
    format!("{stanza}/rendered_md/docs/{blake3}.md")
}

pub fn render_fingerprint(blake3: &str) -> String {
    format!("{blake3}.v{RENDER_VERSION}")
}

pub struct RenderSummary {
    pub converted: usize,
    pub skipped_unchanged: usize,
    pub failed: usize,
}

/// Load the work list. Split from [`render_targets`] so the async
/// database work finishes before the non-`Send` document sink enters
/// scope — otherwise the whole render future is non-`Send` and cannot
/// be driven by the `#[async_trait]` processor.
pub async fn load_targets(raw_dir: &Path) -> Result<Vec<RenderTarget>> {
    let db_path = crate::download::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let db = RawDb::open_reader(&db_path).await?;
    let targets = async {
        match db.scan_root().await? {
            Some(root) => db.convertible_documents(&root).await,
            // No scan has run against this store yet.
            None => Ok(Vec::new()),
        }
    }
    .await;
    // Closed before returning, on the error path too: the next open of
    // this store is a second connection until this one is gone.
    db.close().await;
    targets
}

pub fn render_targets(
    targets: &[RenderTarget],
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        converted: 0,
        skipped_unchanged: 0,
        failed: 0,
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

        // The fingerprint IS the content hash. That is the whole payoff
        // of content identity: a document that has not changed cannot
        // need re-conversion, and one that has changed has a different
        // primary key, so there is no separate invalidation to get
        // wrong.
        let fingerprint = render_fingerprint(&t.blake3);
        if prior_fingerprints.get(&doc_uuid) == Some(&fingerprint) && md_path.exists() {
            summary.skipped_unchanged += 1;
            continue;
        }

        match render_one(t, &md_path, source_name, &doc_uuid) {
            Ok(rendered) => {
                summary.converted += 1;
                on_doc_complete(rendered)?;
            }
            Err(e) => {
                // One malformed document must not abort a corpus scan.
                summary.failed += 1;
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
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let targets = load_targets(raw_dir).await?;
    render_targets(
        &targets,
        out_dir,
        source_name,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )
}

fn render_one(
    t: &RenderTarget,
    md_path: &Path,
    source_name: &str,
    doc_uuid: &str,
) -> Result<RenderedMarkdown> {
    let pages = convert::convert(&t.abs_path)?;
    let title = grid_rows::display_title(t.title.as_deref(), &t.rel_path);

    let qmd_rel = doc_qmd_path_rel(source_name, &t.blake3);

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
        source_name,
    };
    let rows = grid_rows::rows_for_document(&meta, &page_rows);

    Ok(RenderedMarkdown {
        markdown_uuid: doc_uuid.to_string(),
        source_name: source_name.to_string(),
        source_fingerprint: render_fingerprint(&t.blake3),
        upstream_cursor: None,
        md_path: md_path.to_path_buf(),
        render_version: RENDER_VERSION,
        rows,
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
    fn the_fingerprint_changes_when_the_renderer_does() {
        // The whole point: content alone would skip re-rendering after a
        // renderer change, leaving an existing install on stale output
        // forever. If this ever equals the bare hash again, the cache
        // key has lost its version component.
        let fp = render_fingerprint("deadbeef");
        assert_ne!(fp, "deadbeef");
        assert!(fp.starts_with("deadbeef."), "{fp}");
        assert!(fp.ends_with(&RENDER_VERSION.to_string()), "{fp}");
    }

    #[test]
    fn the_fingerprint_still_distinguishes_content() {
        assert_ne!(render_fingerprint("aaaa"), render_fingerprint("bbbb"));
    }

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
        assert_eq!(rel, "tng_pdfs/rendered_md/docs/abc123.md");
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
        let out_dir = datalib_etl::layout::rendered_md_root(root, "tng_pdfs");
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
    /// `Some(set)` → convert only documents whose blake3 is in it.
    /// `None` → cold start: convert everything the corpus holds.
    pub changed: Option<std::collections::HashSet<String>>,
    pub new_head: Option<String>,
    pub elapsed: Option<std::time::Duration>,
}

/// Ask the raw store which documents moved since `last_render_hash`.
///
/// The bucket is the document's blake3, which is also its identity and its
/// render fingerprint — for this provider "changed" and "different
/// document" are the same statement. `pdf_paths` joins the union because a
/// file appearing at a new path is how a document enters the corpus, even
/// when its bytes were already known.
pub async fn scan_changed(raw_dir: &Path, last_render_hash: Option<&str>) -> Result<PdfScan> {
    let db_path = crate::download::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(PdfScan::default());
    }
    // Read-only, and closed before returning: `load_targets` ran just
    // before this against the same file, and a second pool overlapping the
    // first is the "database is locked" hazard `open_reader`'s docs name.
    let db = RawDb::open_reader(&db_path).await?;
    // Pin first: the diff and anything read at it must name one commit, and
    // the views have to exist before the bucket query runs. No commit means
    // nothing committed to scan.
    let Some(pin) = datalib_etl::pin::head(db.pool()).await? else {
        db.close().await;
        return Ok(PdfScan::default());
    };
    datalib_etl::pin::install_views(db.pool(), &pin)
        .await
        .context("pin the pdf raw store for the render scan")?;
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        db.pool(),
        last_render_hash,
        &pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            // `pdf_scan_meta` records where the scan ran, which no
            // rendered document reads.
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
    .await;
    db.close().await;
    let scan = scan?;
    Ok(PdfScan {
        changed: scan.changed_buckets,
        new_head: scan.new_head,
        elapsed: scan.scan_elapsed,
    })
}
