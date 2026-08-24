//! End-to-end over the fixture corpus: scan → store → render.
//!
//! Asserts against the raw store and the emitted markdown rather than
//! against log lines, per AGENTS.md §"Inspecting doltlite stores" — a
//! log line says what the code *said*, the store says what it *did*.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use sqlx::Row;

use datalib_etl_pdf::download::{self, RawDb};
use datalib_etl_pdf::render;

const NOW: &str = "2364-04-13T08:45:00-07:00";

fn fixture_dir() -> PathBuf {
    let rel = std::env::var("PDF_FIXTURE_DIR").expect("PDF_FIXTURE_DIR must be set by the build");
    // Under `bazel test` the runfiles root is CWD; under `cargo test`
    // the env var is repo-relative from the workspace root.
    let p = PathBuf::from(&rel);
    if p.is_dir() {
        return p;
    }
    let up = PathBuf::from("../../../../..").join(&rel);
    assert!(
        up.is_dir(),
        "fixture dir not found: {rel} (cwd {:?})",
        std::env::current_dir()
    );
    up
}

struct Harness {
    _tmp: tempfile::TempDir,
    raw_dir: PathBuf,
    root: PathBuf,
    out_dir: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let raw_dir = tmp.path().join("raw");
        let out_dir = tmp.path().join("rendered_md");
        std::fs::create_dir_all(&raw_dir).unwrap();
        Self {
            root: fixture_dir(),
            _tmp: tmp,
            raw_dir,
            out_dir,
        }
    }

    async fn scan(&self) -> Result<download::FetchSummary> {
        let db = RawDb::open(&download::db_path_for(&self.raw_dir)).await?;
        download::fetch(download::FetchOptions {
            db,
            root: self.root.clone(),
            ignore: vec![],
            max_bytes: None,
            force_rehash: false,
            now: NOW.to_string(),
            progress: datalib_etl::progress::Progress::noop(),
        })
        .await
    }

    async fn render(
        &self,
        prior: &HashMap<String, String>,
    ) -> Result<(
        render::RenderSummary,
        Vec<datalib_etl::grid_index::RenderedMarkdown>,
    )> {
        let mut emitted = Vec::new();
        let mut sink = |md| {
            emitted.push(md);
            Ok(())
        };
        let s = render::render(
            &self.raw_dir,
            &self.root,
            &self.out_dir,
            "logs",
            &datalib_etl::progress::Progress::noop(),
            prior,
            &mut sink,
        )
        .await?;
        Ok((s, emitted))
    }

    async fn db(&self) -> RawDb {
        RawDb::open(&download::db_path_for(&self.raw_dir))
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_dedups_by_content_and_records_scanned_docs() -> Result<()> {
    let h = Harness::new();
    let s = h.scan().await?;

    // 6 .pdf files in the tree; readme.txt must not be among them.
    assert_eq!(s.pdfs_seen, 6, "walker should find exactly the .pdf files");

    // captains_log.pdf and archive/captains_log_copy.pdf are byte-identical,
    // so 6 paths collapse to 5 documents — minus the corrupt one, which
    // fails to identify. 4 real documents.
    let db = h.db().await;
    let n_docs: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_documents")
        .fetch_one(db.pool())
        .await?
        .get("c");
    let n_paths: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_paths")
        .fetch_one(db.pool())
        .await?
        .get("c");
    assert!(
        n_paths > n_docs,
        "duplicate copies must share one document row (paths={n_paths} docs={n_docs})"
    );

    // The duplicate pair points at one blake3.
    let dupes: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM pdf_paths
          WHERE blake3 = (SELECT blake3 FROM pdf_paths WHERE id = 'captains_log.pdf')",
    )
    .fetch_one(db.pool())
    .await?
    .get("c");
    assert_eq!(dupes, 2, "both copies should resolve to the same document");

    // The image-only page is recorded, not silently dropped.
    assert_eq!(s.needs_ocr, 1, "the scanned blueprint must be recorded");
    let scanned: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_documents WHERE needs_ocr = 1")
        .fetch_one(db.pool())
        .await?
        .get("c");
    assert_eq!(scanned, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_columns_are_populated_and_absent_metadata_is_null() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    let r = sqlx::query(
        "SELECT d.title, d.pdf_id_permanent, d.xmp_document_id, d.doc_created_at
           FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
          WHERE p.id = 'captains_log.pdf'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        r.get::<Option<String>, _>("title").as_deref(),
        Some("Captain's Log")
    );
    assert_eq!(
        r.get::<Option<String>, _>("pdf_id_permanent").as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        r.get::<Option<String>, _>("xmp_document_id").as_deref(),
        Some("uuid:enterprise-ncc-1701-d")
    );
    // Offset preserved verbatim, per AGENTS.md §"Timestamp convention".
    assert_eq!(
        r.get::<Option<String>, _>("doc_created_at").as_deref(),
        Some("2364-04-13T08:45:00-07:00")
    );

    // The unlabeled document must come back all-NULL, not error.
    let u = sqlx::query(
        "SELECT d.title, d.pdf_id_permanent, d.xmp_document_id
           FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
          WHERE p.id = 'engineering/unlabeled_schematic.pdf'",
    )
    .fetch_one(db.pool())
    .await?;
    assert!(u.get::<Option<String>, _>("title").is_none());
    assert!(u.get::<Option<String>, _>("pdf_id_permanent").is_none());
    assert!(u.get::<Option<String>, _>("xmp_document_id").is_none());
    Ok(())
}

/// The Ship-of-Theseus case: two distinct content identities that are
/// the same conceptual document, joinable by the lineage column.
#[tokio::test(flavor = "multi_thread")]
async fn revisions_are_distinct_documents_joined_by_lineage() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    let rows = sqlx::query(
        "SELECT blake3, xmp_instance_id FROM pdf_documents
          WHERE xmp_document_id = 'uuid:enterprise-ncc-1701-d'
          ORDER BY xmp_instance_id",
    )
    .fetch_all(db.pool())
    .await?;

    assert_eq!(rows.len(), 2, "v1 and v2 are separate content identities");
    let a: String = rows[0].get("blake3");
    let b: String = rows[1].get("blake3");
    assert_ne!(a, b, "different bytes must be different documents");
    assert_eq!(
        rows[0]
            .get::<Option<String>, _>("xmp_instance_id")
            .as_deref(),
        Some("uuid:instance-0001")
    );
    assert_eq!(
        rows[1]
            .get::<Option<String>, _>("xmp_instance_id")
            .as_deref(),
        Some("uuid:instance-0002")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rescan_reuses_hashes_and_is_idempotent() -> Result<()> {
    let h = Harness::new();
    let first = h.scan().await?;
    assert!(first.hashed > 0);
    assert_eq!(first.reused, 0, "nothing to reuse on a cold scan");

    let second = h.scan().await?;
    // Exactly one file is re-read: `holodeck/corrupt.pdf`, which failed
    // to identify the first time and so never got a `pdf_documents`
    // row. Re-reading it is deliberate — a file that could not be
    // parsed (mid-write, partially synced) should be retried rather
    // than cached as permanently broken.
    assert_eq!(
        second.hashed, 1,
        "only the unidentifiable file should be re-read"
    );
    assert!(second.reused > 0, "the rescan cursor should have hit");

    // Row counts must not drift between identical scans.
    let db = h.db().await;
    let n: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_paths")
        .fetch_one(db.pool())
        .await?
        .get("c");
    assert_eq!(
        n as usize,
        second.pdfs_seen - second.too_large - 1 /* corrupt */
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn render_emits_markdown_with_page_anchors_matching_grid_rows() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let (s, emitted) = h.render(&HashMap::new()).await?;

    assert!(s.converted >= 3, "converted={}", s.converted);
    assert_eq!(s.failed, 0);

    // The load-bearing invariant, checked across every document: each
    // page row's uuid must appear as a section anchor in its markdown,
    // or row→preview navigation silently breaks (see `etl::section`).
    for m in &emitted {
        let body = std::fs::read_to_string(&m.md_path)?;
        assert!(body.contains("provider: pdf"), "frontmatter missing");
        let page_rows: Vec<_> = m.rows.iter().filter(|r| r.kind == "PDF Page").collect();
        assert!(!page_rows.is_empty(), "a rendered doc must have pages");
        for r in &page_rows {
            assert!(
                body.contains(&format!(r#"data-section-uuid="{}""#, r.uuid)),
                "page row {} has no anchor in {}",
                r.uuid,
                m.md_path.display()
            );
        }
        // The sidecar the grid_index step consumes rides alongside.
        assert!(m.md_path.with_extension("grid_rows.json").exists());
    }

    // The two-page v1 log specifically: two page rows and real text.
    // Selected by page count because v2 shares its title.
    let v1 = emitted
        .iter()
        .find(|m| m.rows.iter().filter(|r| r.kind == "PDF Page").count() == 2)
        .expect("the two-page captains log should have rendered");
    let body = std::fs::read_to_string(&v1.md_path)?;
    assert!(body.contains("Deneb IV"), "body text missing");
    assert!(
        !body.contains("Addendum filed"),
        "v1 must not contain v2's third page"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn scanned_documents_are_recorded_but_not_rendered() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let (_, emitted) = h.render(&HashMap::new()).await?;

    // No emitted document may come from the image-only fixture.
    for m in &emitted {
        let body = std::fs::read_to_string(&m.md_path)?;
        assert!(
            !body.contains("scanned_blueprint"),
            "the scanned fixture must not render until OCR lands"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_documents_are_skipped_on_re_render() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let (first, emitted) = h.render(&HashMap::new()).await?;
    assert!(first.converted > 0);

    // Feed back the fingerprints the first pass produced — that is what
    // the orchestrator does between runs.
    let prior: HashMap<String, String> = emitted
        .iter()
        .map(|m| (m.markdown_uuid.clone(), m.source_fingerprint.clone()))
        .collect();

    let (second, _) = h.render(&prior).await?;
    assert_eq!(
        second.converted, 0,
        "nothing changed; nothing should re-convert"
    );
    assert_eq!(second.skipped_unchanged, first.converted);
    Ok(())
}
