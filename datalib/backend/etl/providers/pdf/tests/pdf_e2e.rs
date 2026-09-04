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

/// The stanza name this harness renders under. Both the render's
/// `source_name` argument and the `<stanza>/` prefix of every path it
/// writes, exactly as the step wires them (`processor.rs`).
const STANZA: &str = "logs";

struct Harness {
    _tmp: tempfile::TempDir,
    raw_dir: PathBuf,
    root: PathBuf,
    /// The *data* root — the prefix `grid_index::apply_one` strips off
    /// `md_path` to produce the stored path. The real layout is mirrored
    /// here (`<data_root>/<stanza>/rendered_md/`) rather than flattened,
    /// so a test can compare a stored `qmd_path` against it.
    data_root: PathBuf,
    out_dir: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let raw_dir = tmp.path().join("raw");
        let data_root = tmp.path().join("data");
        let out_dir = datalib_etl::layout::rendered_md_root(&data_root, STANZA);
        std::fs::create_dir_all(&raw_dir).unwrap();
        Self {
            root: fixture_dir(),
            _tmp: tmp,
            raw_dir,
            data_root,
            out_dir,
        }
    }

    async fn scan(&self) -> Result<download::FetchSummary> {
        let db = RawDb::open(&download::db_path_for(&self.raw_dir)).await?;
        download::fetch(download::FetchOptions {
            db,
            source_name: STANZA.to_string(),
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
            &self.out_dir,
            STANZA,
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

    // 8 .pdf files in the tree; readme.txt must not be among them.
    assert_eq!(s.pdfs_seen, 8, "walker should find exactly the .pdf files");

    // captains_log.pdf and archive/captains_log_copy.pdf are byte-identical,
    // so the 8 paths collapse to 7 distinct contents — minus the corrupt
    // one, which fails to identify. 6 real documents, 4 of them
    // convertible (neither scanned blueprint is; the mixed hull survey
    // is, for its one text page).
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

    // The image-only pages are recorded, not silently dropped. Three
    // documents carry one: the blueprint, its retitled twin, and the
    // mixed hull survey — whose *other* page still renders.
    assert_eq!(
        s.needs_ocr, 3,
        "every document with an unreadable page must be on the OCR work list"
    );
    let flagged: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_documents WHERE needs_ocr = 1")
        .fetch_one(db.pool())
        .await?
        .get("c");
    assert_eq!(flagged, 3);
    Ok(())
}

/// Regression for #173: a `Mixed` document — some pages readable, some
/// not — must render the pages that are.
///
/// The rule used to be `needs_ocr = 0`, and a Mixed document always has
/// a non-empty `pages_needing_ocr` (that is what makes it Mixed), so no
/// Mixed document ever rendered. On the fixture corpus that cost one
/// page; on a 200-page report with three scanned inserts it costs 197.
/// The corpus had no Mixed document at all, which is why nothing caught
/// it — hence `engineering/hull_survey.pdf`.
#[tokio::test(flavor = "multi_thread")]
async fn a_mixed_document_renders_its_readable_pages() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    // The premise: this fixture really is the Mixed case, and really is
    // on the OCR work list. Without these the test below could pass on
    // a document that simply had nothing to skip.
    let r = sqlx::query(
        "SELECT d.pdf_type, d.needs_ocr, d.page_count, d.ocr_page_count,
                d.has_encoding_issues
           FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
          WHERE p.id = 'engineering/hull_survey.pdf'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(r.get::<String, _>("pdf_type"), "mixed");
    assert_eq!(r.get::<i64, _>("needs_ocr"), 1, "page 2 needs OCR");
    assert_eq!(r.get::<i64, _>("page_count"), 2);
    assert_eq!(r.get::<i64, _>("ocr_page_count"), 1);
    // Detect-only cannot see garbled *text*, only garbled fonts, and
    // this document has neither. If this ever flips true the render gate
    // suppresses the document and the assertions below stop meaning what
    // they say.
    assert_eq!(r.get::<i64, _>("has_encoding_issues"), 0);

    let (_, emitted) = h.render(&HashMap::new()).await?;
    let survey = emitted
        .iter()
        .find(|m| {
            m.rows
                .iter()
                .any(|x| x.conversation_name.as_deref() == Some("Hull Integrity Survey"))
        })
        .expect("the mixed document must render");

    // Page 1 converted and carries a grid row.
    let page_rows: Vec<_> = survey
        .rows
        .iter()
        .filter(|x| x.kind == "PDF Page")
        .collect();
    assert_eq!(page_rows.len(), 1, "only page 1 has text");
    assert_eq!(page_rows[0].message_index, Some(1));
    assert!(page_rows[0].text.contains("Ablative plating"));

    // Page 2 is in the markdown as a note, not as a hole. The note gets
    // no section anchor, because there is nothing to navigate to.
    let body = std::fs::read_to_string(&survey.md_path)?;
    assert!(body.contains("Ablative plating"), "page 1 body missing");
    assert!(
        body.contains(&datalib_etl_pdf::render::convert::note_for_page(2)),
        "page 2 must leave a note; got:\n{body}"
    );
    assert!(
        !body.contains(&format!(
            r#"data-section-uuid="{}""#,
            datalib_etl_pdf::render::grid_rows::page_uuid(
                &survey.rows[0].upstream_id.clone().unwrap(),
                2
            )
        )),
        "an unreadable page must not get a section anchor"
    );
    Ok(())
}

/// The payoff of `download::content_hash`, asserted against the store
/// rather than against the pure function: two files that differ only in
/// their metadata land as two rows sharing one content identity.
#[tokio::test(flavor = "multi_thread")]
async fn a_metadata_only_variant_shares_one_content_hash() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    let rows = sqlx::query(
        "SELECT p.id AS path, d.blake3 AS blake3, d.content_blake3 AS content_blake3,
                d.title AS title
           FROM pdf_paths p JOIN pdf_documents d ON d.blake3 = p.blake3
          WHERE p.id LIKE 'holodeck/scanned_blueprint%'
          ORDER BY p.id",
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(rows.len(), 2, "the blueprint and its retitled twin");

    let file_hashes: Vec<String> = rows.iter().map(|r| r.get("blake3")).collect();
    let titles: Vec<Option<String>> = rows.iter().map(|r| r.get("title")).collect();
    let content: Vec<Option<String>> = rows.iter().map(|r| r.get("content_blake3")).collect();

    // The premise. Without these two the equality below would be
    // trivially true and would prove nothing.
    assert_ne!(
        file_hashes[0], file_hashes[1],
        "the files really must differ in their bytes"
    );
    assert_ne!(titles[0], titles[1], "and really must differ in metadata");

    assert!(content[0].is_some(), "content hash must be populated");
    assert_eq!(
        content[0], content[1],
        "a retitle and a fresh /ID must not change content identity \
         (titles {titles:?}, file hashes {file_hashes:?})"
    );
    Ok(())
}

/// Every document we could parse gets a content hash; the ones we could
/// not are NULL rather than absent or wrong.
#[tokio::test(flavor = "multi_thread")]
async fn content_hash_is_populated_for_every_parseable_document() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    let missing: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM pdf_documents WHERE content_blake3 IS NULL")
            .fetch_one(db.pool())
            .await?
            .get("c");
    assert_eq!(
        missing, 0,
        "every fixture document is parseable and unencrypted"
    );

    // Distinct documents must not collide. The two blueprints share a
    // content hash by design, so the corpus has one fewer content
    // identity than it has documents.
    let docs: i64 = sqlx::query("SELECT COUNT(*) AS c FROM pdf_documents")
        .fetch_one(db.pool())
        .await?
        .get("c");
    let identities: i64 =
        sqlx::query("SELECT COUNT(DISTINCT content_blake3) AS c FROM pdf_documents")
            .fetch_one(db.pool())
            .await?
            .get("c");
    assert_eq!(
        identities,
        docs - 1,
        "only the retitled blueprint may share an identity ({identities} of {docs})"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_columns_are_populated_and_absent_metadata_is_null() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let db = h.db().await;

    let r = sqlx::query(
        "SELECT d.title, d.author, d.pdf_id_permanent, d.xmp_document_id, d.doc_created_at
           FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
          WHERE p.id = 'captains_log.pdf'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        r.get::<Option<String>, _>("title").as_deref(),
        Some("Captain's Log")
    );
    // From the Info dict's /Author.
    assert_eq!(
        r.get::<Option<String>, _>("author").as_deref(),
        Some("Jean-Luc Picard")
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

    // The document with no Info dict, no /ID and no XMP: every
    // *identity* column must come back NULL rather than the scan
    // failing. `title` is deliberately not asserted NULL here — with no
    // Info title, pdf-inspector may still infer one from the page's
    // largest text, and that inferred value is a better grid label than
    // nothing.
    let u = sqlx::query(
        "SELECT d.author, d.pdf_id_permanent, d.xmp_document_id
           FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
          WHERE p.id = 'engineering/warp_core_manual.pdf'",
    )
    .fetch_one(db.pool())
    .await?;
    assert!(u.get::<Option<String>, _>("pdf_id_permanent").is_none());
    assert!(u.get::<Option<String>, _>("xmp_document_id").is_none());
    // This one has NO Info /Author, only XMP `dc:creator` — so a value
    // here proves the fallback path, not just the Info-dict path.
    assert_eq!(
        u.get::<Option<String>, _>("author").as_deref(),
        Some("Geordi La Forge")
    );
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

    // Exactly the four renderable documents, and 5 pages between
    // them. Pinned rather than bounded: every page here is embedded by
    // the qmd indexer on every full fixture build, so growth should be
    // a deliberate edit, not a silent drift. The mixed survey's second
    // page is deliberately not among them — it converts to a note in
    // the markdown, which costs no row and no embedding.
    assert_eq!(s.converted, 4, "four renderable documents");
    assert_eq!(s.failed, 0);
    let total_pages: usize = emitted
        .iter()
        .map(|m| m.rows.iter().filter(|r| r.kind == "PDF Page").count())
        .sum();
    assert_eq!(total_pages, 5, "the corpus is budgeted at 5 rendered pages");

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

    // The two-page v2 log specifically: it is the revision, so it has
    // the addendum that v1 does not. Selected by page count, since the
    // two revisions share a title.
    let v2 = emitted
        .iter()
        .find(|m| m.rows.iter().filter(|r| r.kind == "PDF Page").count() == 2)
        .expect("the two-page captains log revision should have rendered");
    let body = std::fs::read_to_string(&v2.md_path)?;
    assert!(body.contains("Deneb IV"), "body text missing");
    assert!(body.contains("Addendum filed"), "v2's second page missing");

    // ...and the one-page v1 must NOT have it.
    let v1 = emitted
        .iter()
        .find(|m| {
            m.rows.iter().filter(|r| r.kind == "PDF Page").count() == 1
                && m.rows
                    .iter()
                    .any(|r| r.conversation_name.as_deref() == Some("Captain's Log"))
        })
        .expect("the one-page captains log should have rendered");
    let body1 = std::fs::read_to_string(&v1.md_path)?;
    assert!(
        !body1.contains("Addendum filed"),
        "v1 must not carry v2 content"
    );
    Ok(())
}

/// Regression: `grid_rows.qmd_path` must be the *data-root*-relative
/// path, byte-equal to what `grid_index::apply_one` stores in
/// `markdowns.md_path` for the same file.
///
/// It used to be the out-dir-relative `docs/<blake3>.md`, which is what
/// `GridIndex::new` keyed its rows by while `rows_for_hit` looked hits
/// up by their data-root-relative path. The two could never match, so
/// every qmd hit inside a PDF resolved to zero grid rows and was
/// dropped — PDFs were simply absent from free-text search, with only
/// an applet-side `qmd hit resolved to no grid rows` error to show for
/// it. Comparing against the path derived from `md_path` (rather than
/// against a hardcoded string) is the point: it is the same derivation
/// the index performs, so the two cannot drift apart again.
#[tokio::test(flavor = "multi_thread")]
async fn every_qmd_path_equals_its_markdowns_md_path() -> Result<()> {
    let h = Harness::new();
    h.scan().await?;
    let (_, emitted) = h.render(&HashMap::new()).await?;
    assert!(
        !emitted.is_empty(),
        "nothing rendered; the test proves nothing"
    );

    for m in &emitted {
        // Exactly what `apply_one` does to reach `markdowns.md_path`.
        let stored_md_path = m
            .md_path
            .strip_prefix(&h.data_root)
            .expect("rendered files must live under the data root")
            .to_string_lossy()
            .to_string();
        assert!(
            stored_md_path.starts_with(&format!("{STANZA}/rendered_md/")),
            "md_path {stored_md_path} is not under the stanza's rendered_md tree"
        );
        assert!(!m.rows.is_empty(), "a rendered doc must emit rows");
        for r in &m.rows {
            assert_eq!(
                r.qmd_path.as_deref(),
                Some(stored_md_path.as_str()),
                "{} row {} points at a different path than its markdown",
                r.kind,
                r.uuid
            );
        }
    }
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
