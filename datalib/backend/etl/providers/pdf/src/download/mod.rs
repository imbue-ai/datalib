//! The `pdf` download side: walk a tree, identify the PDFs in it, and
//! record what each one *is* — without converting anything.
//!
//! Splitting identification from conversion is what lets the first pass
//! ship without an OCR engine. Classification is cheap and total: every
//! PDF gets a row, including the scanned ones we cannot read yet, which
//! land with `needs_ocr = 1`. Adding an engine later is then a pure
//! addition — the work list already exists as a SQL query
//! (`SELECT … WHERE needs_ocr = 1`) instead of needing a re-scan.

pub mod db;
pub mod identity;
pub mod schema_raw;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::fswalk::{self, StampDecision};
use datalib_etl::progress::Progress;

pub use db::{db_path_for, RawDb, RenderTarget};
use schema_raw::{PdfDocumentRow, PdfKind, PdfPathRow, PdfScanMetaRow};

/// Rows are flushed to the store in batches of this size so a long scan
/// is resumable-ish and memory stays bounded. Document corpora are
/// small enough that this is rarely reached.
const BATCH_SIZE: usize = 2_000;

pub struct FetchOptions {
    pub db: RawDb,
    /// Source name from config, used as the `pdf_scan_meta` key.
    pub source_name: String,
    /// Tree to scan.
    pub root: PathBuf,
    pub ignore: Vec<String>,
    pub max_bytes: Option<u64>,
    /// Ignore the rescan cache and re-read every file. Wired to the
    /// framework's `--reset-and-redownload`.
    pub force_rehash: bool,
    /// Run-pinned "now", per AGENTS.md — steps prefer `DATALIB_DAG_NOW`
    /// over sampling their own clock so one run's outputs agree.
    pub now: String,
    pub progress: Progress,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub pdfs_seen: usize,
    /// Files whose bytes we actually read and hashed.
    pub hashed: usize,
    /// Files skipped via the `(mtime, size, inode, dev)` cursor.
    pub reused: usize,
    /// Distinct documents (by content) behind those paths.
    pub documents: usize,
    /// Documents classified as needing OCR — recorded, not converted.
    pub needs_ocr: usize,
    /// Paths skipped for exceeding `max_bytes`.
    pub too_large: usize,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let mut summary = FetchSummary::default();

    // Load the cache BEFORE truncating the path table, exactly as
    // fsindex does — the truncate is what makes deletions fall out, and
    // the in-memory cache is what preserves the fast-rescan path across
    // it.
    let prev = opts.db.load_prev().await.context("load rescan cache")?;
    opts.db.reset_paths().await.context("reset pdf_paths")?;

    // Written before the walk, so an interrupted scan still leaves the
    // render step able to find the tree.
    opts.db
        .write_scan_meta(&PdfScanMetaRow {
            id: opts.source_name.clone(),
            abs_root: opts.root.to_string_lossy().to_string(),
            scanned_at: opts.now.clone(),
        })
        .await
        .context("record scan root")?;

    let (files, walk_errors) = fswalk::walk_files(&opts.root, &opts.ignore, is_pdf)
        .with_context(|| format!("walk {}", opts.root.display()))?;
    summary.errors += walk_errors.len();
    for e in &walk_errors {
        tracing::warn!(path = %e.path.display(), error = %e.error, "pdf_walk_error");
    }

    summary.pdfs_seen = files.len();
    opts.progress.set_length(Some(files.len() as u64));

    let mut doc_batch: Vec<PdfDocumentRow> = Vec::new();
    let mut path_batch: Vec<PdfPathRow> = Vec::new();
    // Documents identified during *this* scan, so N copies of one file
    // are classified once rather than N times.
    let mut seen_docs: HashMap<String, bool> = HashMap::new();

    for f in files {
        opts.progress.inc(1);

        let fresh = fswalk::fresh_stat(&f.meta);
        if let Some(max) = opts.max_bytes {
            if fresh.size as u64 > max {
                summary.too_large += 1;
                tracing::info!(path = %f.rel, size = fresh.size, "pdf_skipped_too_large");
                continue;
            }
        }

        // ── Reuse or rehash ──────────────────────────────────────────
        let cached = prev.paths.get(&f.rel);
        let decision = if opts.force_rehash {
            StampDecision::Rehash
        } else {
            fswalk::decide(cached.map(|(c, _)| c), &fresh)
        };

        let hash_hex = match decision {
            StampDecision::ReuseHash => {
                // Only safe when the document row survives too;
                // otherwise we'd record a path pointing at nothing.
                let (_, h) = cached.expect("ReuseHash implies a cache entry");
                if prev.known_docs.contains(h) || seen_docs.contains_key(h) {
                    summary.reused += 1;
                    h.clone()
                } else {
                    match hash_and_count(&f.path, fresh.size as u64, &mut summary) {
                        Some(h) => h,
                        None => continue,
                    }
                }
            }
            StampDecision::Rehash => match hash_and_count(&f.path, fresh.size as u64, &mut summary)
            {
                Some(h) => h,
                None => continue,
            },
        };

        // ── Classify the document, once per distinct content ─────────
        if !prev.known_docs.contains(&hash_hex) && !seen_docs.contains_key(&hash_hex) {
            match identify(&f.path, fresh.size, &opts.now) {
                Ok(row) => {
                    let needs_ocr = row.needs_ocr;
                    seen_docs.insert(hash_hex.clone(), needs_ocr);
                    summary.documents += 1;
                    if needs_ocr {
                        summary.needs_ocr += 1;
                    }
                    doc_batch.push(PdfDocumentRow {
                        blake3: hash_hex.clone(),
                        ..row
                    });
                }
                Err(e) => {
                    summary.errors += 1;
                    tracing::warn!(path = %f.rel, error = %e, "pdf_identify_failed");
                    continue;
                }
            }
        }

        path_batch.push(PdfPathRow {
            id: f.rel.clone(),
            blake3: hash_hex,
            mtime_ns: fresh.mtime_ns,
            size: fresh.size,
            stamp_kind: fswalk::stamp_kind_for(&fresh),
            inode: fresh.inode,
            dev: fresh.dev,
            last_seen_at: opts.now.clone(),
        });

        if path_batch.len() >= BATCH_SIZE {
            opts.db.write_batch(&doc_batch, &path_batch).await?;
            doc_batch.clear();
            path_batch.clear();
        }
    }

    if !doc_batch.is_empty() || !path_batch.is_empty() {
        opts.db.write_batch(&doc_batch, &path_batch).await?;
    }
    Ok(summary)
}

fn is_pdf(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

fn hash_and_count(path: &Path, size: u64, summary: &mut FetchSummary) -> Option<String> {
    match fswalk::hash_file(path, size) {
        Ok(h) => {
            summary.hashed += 1;
            Some(fswalk::to_hex(&h))
        }
        Err(e) => {
            summary.errors += 1;
            tracing::warn!(path = %path.display(), error = %e, "pdf_hash_failed");
            None
        }
    }
}

/// Classify one PDF and read its metadata. Returns a row with an empty
/// `blake3` — the caller fills that in, since it already has the digest.
fn identify(path: &Path, size: i64, now: &str) -> Result<PdfDocumentRow> {
    // Detect-only: we want the classification and page census here, not
    // the markdown. Conversion is the render step's job and happens
    // against a different cache key.
    let det =
        pdf_inspector::process_pdf_with_options(path, pdf_inspector::PdfOptions::detect_only())
            .map_err(|e| anyhow::anyhow!("classify {}: {e}", path.display()))?;

    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let ident = identity::extract(&bytes);

    let kind = match det.pdf_type {
        pdf_inspector::PdfType::TextBased => PdfKind::TextBased,
        pdf_inspector::PdfType::Scanned => PdfKind::Scanned,
        pdf_inspector::PdfType::ImageBased => PdfKind::ImageBased,
        pdf_inspector::PdfType::Mixed => PdfKind::Mixed,
    };

    // "Needs OCR" is about whether we can produce useful text *now*, so
    // it folds in three things the classifier reports separately: an
    // unconvertible type, pages with no text operators, and broken font
    // encodings (which yield mojibake that looks like text but isn't).
    let needs_ocr = !kind.is_convertible_without_ocr()
        || !det.pages_needing_ocr.is_empty()
        || det.has_encoding_issues;

    Ok(PdfDocumentRow {
        blake3: String::new(),
        size,
        page_count: i64::from(det.page_count),
        pdf_type: kind,
        confidence: f64::from(det.confidence),
        needs_ocr,
        ocr_page_count: det.pages_needing_ocr.len() as i64,
        has_encoding_issues: det.has_encoding_issues,
        // Prefer the PDF's own Info-dict title; fall back to whatever
        // the extractor inferred from the page.
        title: ident.title.clone().or(det.title),
        doc_created_at: ident.created_at,
        doc_modified_at: ident.modified_at,
        pdf_id_permanent: ident.pdf_id_permanent,
        xmp_document_id: ident.xmp_document_id,
        xmp_instance_id: ident.xmp_instance_id,
        xmp_original_document_id: ident.xmp_original_document_id,
        first_seen_at: now.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_extension_match_is_case_insensitive() {
        assert!(is_pdf(Path::new("a.pdf")));
        assert!(is_pdf(Path::new("a.PDF")));
        assert!(is_pdf(Path::new("a.Pdf")));
        assert!(!is_pdf(Path::new("a.txt")));
        assert!(!is_pdf(Path::new("pdf")));
    }
}
