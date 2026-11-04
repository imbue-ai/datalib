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
use datalib_etl_files::file_changes_scan;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
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
    fetch_with(opts, identify).await
}

/// What reads one PDF into its row: [`identify`], or in a test, one
/// that panics.
type Identify = fn(&Path, &[u8]) -> Result<PdfDocumentRow>;

async fn fetch_with(opts: FetchOptions, identify: Identify) -> Result<FetchSummary> {
    let pool = opts.db.pool().clone();
    // The scan has no stop to honour, so it always covers the whole tree.
    let never_stops = StopFlag::new();
    run_problems::collecting(&pool, &never_stops, |found| {
        scan_tree(opts, found, identify)
    })
    .await
}

async fn scan_tree(
    opts: FetchOptions,
    found: RunProblems,
    identify: Identify,
) -> Result<FetchSummary> {
    let mut summary = FetchSummary::default();

    // Load the cache BEFORE truncating the path table, exactly as
    // file_inventory does — the truncate is what makes deletions fall out, and
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

    // One walk and no read: a file the host's shared cache can vouch for,
    // because a `media` or `file_inventory` scan of the same tree paid for it or
    // because nothing has touched it, is known by its hash. The rest are read
    // below, once, for both their hash and their classification.
    let scan = file_changes_scan::list(
        &opts.cache,
        &opts.root,
        &file_changes_scan::ScanOptions {
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
    opts.progress.set_length(Some(scan.files.len() as u64));

    let mut doc_batch: Vec<PdfDocumentRow> = Vec::new();
    // Documents identified during *this* scan, so N copies of one file
    // are classified once rather than N times.
    let mut seen_docs: HashSet<String> = HashSet::new();

    for f in &scan.files {
        opts.progress.inc(1);
        // ── Classify the document, once per distinct content ─────────
        // A hash from the cache only decides whether to look. A document
        // read is named by the hash of the bytes it was classified from.
        let held = |hash: &String| prev.known_docs.contains(hash) || seen_docs.contains(hash);
        let known = f.known.map(|hash| fswalk::to_hex(&hash)).filter(held);
        let hash_hex = if let Some(hash) = known {
            summary.reused += 1;
            hash
        } else {
            // A file the cache vouches for and no document holds, because it
            // never classified, is read again without needing its hash.
            if f.known.is_some() {
                summary.reused += 1;
            } else {
                summary.hashed += 1;
            }
            let file = match scan.read(f).await {
                Ok(file) => file,
                Err(e) => {
                    summary.errors += 1;
                    found.record_failed("pdf_paths", &f.rel, format!("{e:#}"));
                    // Still there, so it keeps the path row the last scan wrote.
                    if let Some(hash) = prev.paths.get(&f.rel) {
                        path_batch.push(PdfPathRow {
                            id: f.rel.clone(),
                            blake3: hash.clone(),
                        });
                    }
                    continue;
                }
            };
            let read_hash = fswalk::to_hex(&file.file.blake3);
            if held(&read_hash) {
                read_hash
            } else {
                // lopdf and pdf-inspector read untrusted bytes, so a panic
                // in either is this file's failure, not the scan's.
                match run_problems::catching_panics(|| identify(&f.path, &file.bytes)) {
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
    scan.finish().await?;
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

/// Classify one PDF and read its metadata, and name it by the hash of the
/// bytes.
fn identify(_path: &Path, bytes: &[u8]) -> Result<PdfDocumentRow> {
    // Detect-only: we want the classification and page census here, not
    // the markdown. Conversion is the render step's job and happens
    // against a different cache key.
    let det = pdf_inspector::process_pdf_mem_with_options(
        bytes,
        pdf_inspector::PdfOptions::detect_only(),
    )
    .map_err(|e| anyhow::anyhow!("classify: {e}"))?;

    // One parse feeds both: the metadata fields and the content hash
    // want the same `lopdf::Document`, and building it is the expensive
    // half of each.
    let (ident, content_blake3) = identity::extract_with_content_hash(bytes);

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
        blake3: blake3_hex(bytes),
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
    use super::content_hash::tests::doc;
    use super::*;

    fn panics_on_red_alert(path: &Path, bytes: &[u8]) -> Result<PdfDocumentRow> {
        if path.ends_with("red_alert.pdf") {
            panic!("a parser met bytes it did not expect");
        }
        identify(path, bytes)
    }

    async fn scan(root: &Path, raw: &Path, db: &RawDb, identify: Identify) -> FetchSummary {
        let cache = FingerprintCache::open(&raw.join("fingerprints.sqlite"))
            .await
            .unwrap();
        let summary = fetch_with(
            FetchOptions {
                db: db.clone(),
                source_id: "tng_pdf".into(),
                root: root.to_path_buf(),
                ignore: vec![],
                max_bytes: None,
                cache: cache.clone(),
                now: "2364-04-13T08:45:00-07:00".into(),
                progress: Progress::noop(),
            },
            identify,
        )
        .await
        .unwrap();
        cache.pool().close().await;
        summary
    }

    async fn column(db: &RawDb, sql: &'static str) -> Vec<String> {
        sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap()
    }

    /// A parser that panicked on one PDF ended the whole scan, and the
    /// file was never stored, so every later run met it again and the
    /// source never synced another file.
    #[tokio::test]
    async fn a_file_whose_parser_panics_costs_only_that_file() {
        let dir = tempfile::tempdir().unwrap();
        let (root, raw) = (dir.path().join("logs"), dir.path().join("raw"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&raw).unwrap();
        for (name, body) in [
            ("bridge.pdf", "warp core nominal"),
            ("red_alert.pdf", "shields up"),
            ("ten_forward.pdf", "tea, Earl Grey, hot"),
        ] {
            std::fs::write(root.join(name), doc(body, name, "01", None)).unwrap();
        }
        let db = RawDb::open(&db_path_for(&raw)).await.unwrap();

        let summary = scan(&root, &raw, &db, panics_on_red_alert).await;
        assert_eq!((summary.documents, summary.errors), (2, 1));
        assert_eq!(
            column(&db, "SELECT id FROM pdf_paths ORDER BY id").await,
            ["bridge.pdf", "ten_forward.pdf"]
        );
        let problems: Vec<(String, String)> =
            sqlx::query_as("SELECT scope_key, sample FROM problems")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            problems,
            [(
                "record:pdf_paths:red_alert.pdf".to_string(),
                "panicked: a parser met bytes it did not expect".to_string()
            )]
        );

        scan(&root, &raw, &db, identify).await;
        assert_eq!(
            column(&db, "SELECT id FROM pdf_paths ORDER BY id").await,
            ["bridge.pdf", "red_alert.pdf", "ten_forward.pdf"],
            "the next run tries the file again"
        );
        assert!(column(&db, "SELECT scope_key FROM problems")
            .await
            .is_empty());
        db.pool().close().await;
    }

    /// A PDF that will not open is still there: truncating `pdf_paths` lost
    /// its path, and the prune then lost its document, until it opened again.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_that_will_not_open_keeps_its_path_and_document() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (root, raw) = (dir.path().join("logs"), dir.path().join("raw"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&raw).unwrap();
        for (name, body) in [("bridge.pdf", "warp core"), ("sickbay.pdf", "tricorder")] {
            std::fs::write(root.join(name), doc(body, name, "01", None)).unwrap();
        }
        let db = RawDb::open(&db_path_for(&raw)).await.unwrap();
        scan(&root, &raw, &db, identify).await;

        let locked = root.join("sickbay.pdf");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            // Root reads through any mode; CI's container runs as root.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
            db.pool().close().await;
            return;
        }
        let summary = scan(&root, &raw, &db, identify).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(summary.errors, 1);
        assert_eq!(
            column(&db, "SELECT id FROM pdf_paths ORDER BY id").await,
            ["bridge.pdf", "sickbay.pdf"]
        );
        assert_eq!(
            column(&db, "SELECT blake3 FROM pdf_documents").await.len(),
            2
        );
        db.pool().close().await;
    }

    #[test]
    fn pdf_extension_match_is_case_insensitive() {
        assert!(is_pdf(Path::new("a.pdf")));
        assert!(is_pdf(Path::new("a.PDF")));
        assert!(is_pdf(Path::new("a.Pdf")));
        assert!(!is_pdf(Path::new("a.txt")));
        assert!(!is_pdf(Path::new("pdf")));
    }
}
