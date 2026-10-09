//! The `pdf` download side: walk a tree, identify the PDFs in it, and
//! record what each one *is* — without converting anything.

pub mod content_hash;
pub mod db;
pub mod identity;
pub mod schema_raw;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::blob_cas::blake3_hex;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_files::fsscan;
use datalib_etl_files::fswalk;

pub use db::{db_path_for, RawDb, RenderTarget};
use schema_raw::{PdfDocumentRow, PdfKind, PdfPathRow, PdfScanMetaRow};

/// Rows are flushed to the store in batches of this size so a long scan
/// is resumable-ish and memory stays bounded. Document corpora are
/// small enough that this is rarely reached.
const BATCH_SIZE: usize = 2_000;

pub struct FetchOptions {
    pub db: RawDb,
    /// The source's id, used as the `pdf_scan_meta` key.
    pub source_id: String,
    /// Tree to scan.
    pub root: PathBuf,
    pub ignore: Vec<String>,
    pub max_bytes: Option<u64>,
    /// This host's shared fingerprint cache. Host state, so it lives
    /// outside the scan store — see [`datalib_etl_files::fingerprint_cache`].
    pub cache: FingerprintCache,
    /// Run-pinned "now", per AGENTS.md — steps prefer `DATALIB_DAG_NOW`
    /// over sampling their own clock so one run's outputs agree. Every
    /// stamp this scan writes is this instant, as UTC with its offset
    /// beside it.
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
    /// Documents with at least one page an OCR engine would have to
    /// read. Not the same as "skipped": a document can be on this list
    /// and still render, for the pages that do carry text.
    pub needs_ocr: usize,
    /// Paths skipped for exceeding `max_bytes`.
    pub too_large: usize,
    /// Documents no path names any more.
    pub documents_removed: usize,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let pool = opts.db.pool().clone();
    // The scan has no stop to honour, so it always covers the whole tree.
    let never_stops = StopFlag::new();
    run_problems::collecting(&pool, &never_stops, |found| scan_tree(opts, found)).await
}

async fn scan_tree(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let mut summary = FetchSummary::default();

    // Load the cache BEFORE truncating the path table, exactly as
    // fsindex does — the truncate is what makes deletions fall out, and
    // the in-memory cache is what preserves the fast-rescan path across
    // it.
    let prev = opts.db.load_prev().await.context("load rescan cache")?;

    // Written before the walk, so an interrupted scan still leaves the
    // render step able to find the tree.
    let now = datalib_time::parse_strict(&opts.now)
        .with_context(|| format!("parse the run's now {:?}", opts.now))?;
    opts.db
        .write_scan_meta(
            &PdfScanMetaRow {
                id: opts.source_id.clone(),
                abs_root: opts.root.to_string_lossy().to_string(),
            },
            &now,
        )
        .await
        .context("record scan root")?;

    // One walk, hashing only what this host's shared cache cannot
    // vouch for — so a `media` or `fsindex` scan of the same tree has
    // already paid for most of it.
    let scan = fsscan::scan(
        &opts.cache,
        &opts.root,
        &fsscan::ScanOptions {
            ignore: opts.ignore.clone(),
            max_bytes: opts.max_bytes,
            ..Default::default()
        },
        is_pdf,
    )
    .await?;
    summary.errors += scan.errors.len();
    scan.report_problems(&found, "files");
    // A walk that could not read part of the tree may only have failed to
    // see a path, so the table is not truncated and nothing falls out:
    // what the walk did see is upserted over what was there.
    if scan.errors.is_empty() {
        opts.db.reset_paths().await.context("reset pdf_paths")?;
    }
    // A file the scan found and did not read (over `max_bytes`) is still
    // there, so it keeps the path row the last scan wrote.
    let mut path_batch: Vec<PdfPathRow> = scan
        .present_unread
        .iter()
        .filter_map(|rel| {
            Some(PdfPathRow {
                id: rel.clone(),
                blake3: prev.paths.get(rel)?.clone(),
            })
        })
        .collect();
    summary.pdfs_seen = scan.files.len();
    summary.too_large = scan.stats.too_large;
    summary.hashed = scan.stats.hashed;
    summary.reused = scan.stats.reused;
    opts.progress.set_length(Some(scan.files.len() as u64));

    let mut doc_batch: Vec<PdfDocumentRow> = Vec::new();
    // Documents identified during *this* scan, so N copies of one file
    // are classified once rather than N times.
    let mut seen_docs: HashSet<String> = HashSet::new();

    for f in &scan.files {
        opts.progress.inc(1);
        let scanned = fswalk::to_hex(&f.blake3);

        // ── Classify the document, once per distinct content ─────────
        // The scan's hash only decides whether to look. A document read
        // is named by the hash of the bytes it was classified from, which
        // differ when the file changed after the scan or the scan's
        // cached hash was stale.
        let hash_hex = if prev.known_docs.contains(&scanned) || seen_docs.contains(&scanned) {
            scanned
        } else {
            match identify(&f.path) {
                Ok(row) => {
                    let read = row.blake3.clone();
                    if seen_docs.insert(read.clone()) {
                        summary.documents += 1;
                        if row.needs_ocr {
                            summary.needs_ocr += 1;
                        }
                        doc_batch.push(row);
                    }
                    read
                }
                // Retried every scan: a document that never identified
                // is not in `pdf_documents`.
                Err(e) => {
                    summary.errors += 1;
                    found.record_failed("pdf_paths", &f.rel, format!("{e:#}"));
                    continue;
                }
            }
        };

        path_batch.push(PdfPathRow {
            id: f.rel.clone(),
            blake3: hash_hex,
        });

        if path_batch.len() >= BATCH_SIZE {
            opts.db.write_batch(&doc_batch, &path_batch, &now).await?;
            doc_batch.clear();
            path_batch.clear();
        }
    }

    if !doc_batch.is_empty() || !path_batch.is_empty() {
        opts.db.write_batch(&doc_batch, &path_batch, &now).await?;
    }
    // A document is reached only through a path, so after a clean walk one
    // no path names is gone from the tree. A file moved within it is named
    // at its new path by now, and keeps its document.
    if scan.errors.is_empty() {
        summary.documents_removed = opts.db.prune_unnamed().await? as usize;
    }
    // Every scan retries every document it could not identify, so a row
    // stands only on a path under an entry the walk could not read.
    found.records_tried_all_but("pdf_paths", scan.unseen());
    Ok(summary)
}

fn is_pdf(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Classify one PDF and read its metadata, from one read of the file,
/// and name it by the hash of those bytes.
fn identify(path: &Path) -> Result<PdfDocumentRow> {
    let bytes = std::fs::read(path).context("read")?;
    // Detect-only: we want the classification and page census here, not
    // the markdown. Conversion is the render step's job and happens
    // against a different cache key.
    let det = pdf_inspector::process_pdf_mem_with_options(
        &bytes,
        pdf_inspector::PdfOptions::detect_only(),
    )
    .map_err(|e| anyhow::anyhow!("classify: {e}"))?;

    // One parse feeds both: the metadata fields and the content hash
    // want the same `lopdf::Document`, and building it is the expensive
    // half of each.
    let (ident, content_blake3) = identity::extract_with_content_hash(&bytes);

    let kind = match det.pdf_type {
        pdf_inspector::PdfType::TextBased => PdfKind::TextBased,
        pdf_inspector::PdfType::Scanned => PdfKind::Scanned,
        pdf_inspector::PdfType::ImageBased => PdfKind::ImageBased,
        pdf_inspector::PdfType::Mixed => PdfKind::Mixed,
    };

    // Deliberately *inclusive*: true when any page is unreadable, whether one
    // scanned insert or all 200. It is the work list for an OCR engine we have
    // not built yet, NOT the render gate — that is
    // [`schema_raw::document_is_renderable`], per document. Conflating the two
    // is what issue #173 was.
    let needs_ocr = !det.pages_needing_ocr.is_empty();

    // Which pages we cannot read, and whether any is unreadable because its
    // *font* is broken rather than because it is an image. That separates a
    // gap from mojibake: a scanned page yields nothing and can be noted, while
    // a page decoding to garbage would be indexed as if it meant something.
    let has_encoding_issues = det.ocr_reasons_by_page.iter().any(|p| {
        p.reasons
            .iter()
            .any(|r| r == pdf_inspector::OCR_REASON_SUSPECTED_GARBLED_TEXT)
    });

    Ok(PdfDocumentRow {
        blake3: blake3_hex(&bytes),
        size: bytes.len() as i64,
        page_count: i64::from(det.page_count),
        pdf_type: kind,
        confidence: f64::from(det.confidence),
        needs_ocr,
        ocr_page_count: det.pages_needing_ocr.len() as i64,
        has_encoding_issues,
        // Prefer the PDF's own Info-dict title; fall back to whatever
        // the extractor inferred from the page.
        title: ident.title.clone().or(det.title),
        author: ident.author,
        doc_created_at: ident.created_at,
        doc_modified_at: ident.modified_at,
        content_blake3,
        pdf_id_permanent: ident.pdf_id_permanent,
        xmp_document_id: ident.xmp_document_id,
        xmp_instance_id: ident.xmp_instance_id,
        xmp_original_document_id: ident.xmp_original_document_id,
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
