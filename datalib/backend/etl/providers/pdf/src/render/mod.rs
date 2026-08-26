//! The `pdf` render side: convert each identified document to markdown
//! and emit it with its `grid_rows` sidecar.
//!
//! Only documents the download step marked convertible are read here
//! (`needs_ocr = 0`). Scanned documents already have rows in
//! `pdf_documents`; they simply produce no markdown yet.

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

/// Rendered files live at `<root>/<stanza>/rendered_md/docs/<blake3>.md`.
///
/// Named by content hash rather than by source filename, which is the
/// visible consequence of content identity: two copies of one paper
/// produce one file, and renaming the PDF does not orphan it. The
/// human-readable title rides in the frontmatter and the grid row.
fn md_path_for(out_dir: &Path, blake3: &str) -> PathBuf {
    out_dir.join("docs").join(format!("{blake3}.md"))
}

/// The render cache key for one document: its content hash *and* the
/// renderer that would produce the output.
///
/// Content alone is not enough. A change to the markdown or to the
/// `grid_rows` projection leaves every document's bytes untouched, so a
/// pure-blake3 fingerprint would skip them all and an existing install
/// would keep pre-change output indefinitely. Folding
/// [`RENDER_VERSION`] in is what makes bumping it mean something — see
/// that constant's docs for why the framework's `renderer_version`
/// column cannot be relied on for this.
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
///
/// The scan root comes from `pdf_scan_meta`, not from render config:
/// render converts exactly the tree the download step walked, and the
/// two cannot drift.
pub async fn load_targets(raw_dir: &Path) -> Result<Vec<RenderTarget>> {
    let db_path = crate::download::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let db = RawDb::open(&db_path).await?;
    let Some(root) = db.scan_root().await? else {
        // No scan has run against this store yet.
        return Ok(Vec::new());
    };
    db.convertible_documents(&root).await
}

/// Convert every target and emit it. Synchronous: conversion is
/// CPU-bound and there is nothing to await.
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

        match render_one(t, &md_path, out_dir, source_name, &doc_uuid) {
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
    out_dir: &Path,
    source_name: &str,
    doc_uuid: &str,
) -> Result<RenderedMarkdown> {
    let pages = convert::convert(&t.abs_path)?;
    let title = grid_rows::display_title(t.title.as_deref(), &t.rel_path);

    let qmd_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(md_path)
        .to_string_lossy()
        .to_string();

    let mut body = String::new();
    body.push_str("---\n");
    body.push_str("provider: pdf\n");
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

    datalib_index_lib::emit_sidecar(
        &md_path.with_extension("grid_rows.json"),
        doc_uuid,
        &render_fingerprint(&t.blake3),
        RENDER_VERSION,
        &rows,
        &[],
    )?;

    Ok(RenderedMarkdown {
        markdown_uuid: doc_uuid.to_string(),
        source_name: source_name.to_string(),
        source_fingerprint: render_fingerprint(&t.blake3),
        upstream_cursor: None,
        md_path: md_path.to_path_buf(),
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
    })
}

/// Minimal YAML scalar quoting for frontmatter values.
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
    fn yaml_quoting_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_str(r#"a"b"#), r#""a\"b""#);
        assert_eq!(yaml_str(r"a\b"), r#""a\\b""#);
    }
}
