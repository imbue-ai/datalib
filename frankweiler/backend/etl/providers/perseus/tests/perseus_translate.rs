//! End-to-end render test against the checked-in tiny TEI fixture.
//! Feeds two TEI editions (Greek + English) through `parse + render_all`
//! and asserts the rendered tree shape — file paths, sidecar JSON
//! structure, key text fragments. This is the regression net for the
//! multi-edition render path: if the parser silently drops a section
//! or the renderer flips a UUID derivation, this fails before bad data
//! hits a user's root.
//!
//! The fixture has two editions, `perseus-grc2` (Greek) and
//! `1st1K-eng1` (English), with one section (1.1.2) deliberately
//! missing on the English side. No `__cts__.xml` is present, so edition
//! titles fall back to the short id.

use std::collections::HashMap;
use std::path::PathBuf;

use frankweiler_etl::grid_index::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_perseus::render::align::PerseusAlignments;
use frankweiler_etl_perseus::render::{parse, render};
use frankweiler_etl_perseus::{book_uuid, chapter_uuid, paragraph_uuid};

const GRC: &str = "perseus-grc2";
const ENG: &str = "1st1K-eng1";

fn fixture_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PERSEUS_FIXTURE_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/perseus_tiny")
}

fn render_fixture(
    parsed: &parse::ParsedPerseus,
    out: &std::path::Path,
) -> (render::RenderSummary, Vec<RenderedMarkdown>) {
    let mut emitted: Vec<RenderedMarkdown> = Vec::new();
    let summary = render::render_all(
        parsed,
        &PerseusAlignments::default(),
        out,
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |r: RenderedMarkdown| {
            emitted.push(r);
            Ok(())
        },
    )
    .expect("render");
    (summary, emitted)
}

#[test]
fn renders_all_books_chapters_and_editions() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).expect("parse tiny fixture");

    // 2 editions; grc sorts first.
    assert_eq!(parsed.editions.len(), 2);
    assert_eq!(parsed.editions[0].id, GRC);
    assert_eq!(parsed.editions[1].id, ENG);
    // 2 books × 2 chapters (union of both editions).
    assert_eq!(parsed.books.len(), 2);
    assert_eq!(parsed.books[0].chapters.len(), 2);
    assert_eq!(parsed.books[1].chapters.len(), 2);

    let (summary, emitted) = render_fixture(&parsed, out.path());

    // 2 books + 4 chapters × 2 editions = 10 docs.
    assert_eq!(summary.markdowns_total, 10);
    assert_eq!(summary.markdowns_rendered, 10);
    assert_eq!(summary.markdowns_skipped, 0);
    assert_eq!(emitted.len(), 10);

    let book1 = out
        .path()
        .join("perseus/rendered_md/thucydides/histories/book_01/index.md");
    assert!(book1.exists(), "missing {}", book1.display());

    // Book 1 chapter 1, Greek side has both sections. Unaligned
    // editions render section text verbatim (no per-sentence spans).
    let ch11_grc = out.path().join(format!(
        "perseus/rendered_md/thucydides/histories/book_01/chapter_001_{GRC}.md"
    ));
    let body = std::fs::read_to_string(&ch11_grc).unwrap();
    assert!(body.contains("Θουκυδίδης Ἀθηναῖος ξυνέγραψε."));
    assert!(body.contains("### 1.1.1"));
    assert!(body.contains("### 1.1.2"));
    assert!(!body.contains("<span data-section-uuid"));

    // English side: section 2 is the deliberate translation gap.
    let ch11_eng = out.path().join(format!(
        "perseus/rendered_md/thucydides/histories/book_01/chapter_001_{ENG}.md"
    ));
    let body_eng = std::fs::read_to_string(&ch11_eng).unwrap();
    assert!(body_eng.contains("the Athenian wrote"));
    assert!(body_eng.contains("### 1.1.1"));
    assert!(!body_eng.contains("### 1.1.2"));

    // Book 2 chapter 2: bare chapter text → section "1".
    let ch22_grc = out.path().join(format!(
        "perseus/rendered_md/thucydides/histories/book_02/chapter_002_{GRC}.md"
    ));
    let body22 = std::fs::read_to_string(&ch22_grc).unwrap();
    assert!(body22.contains("### 2.2.1"));
}

#[test]
fn sidecars_carry_stable_uuids_and_provider_metadata() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();
    render_fixture(&parsed, out.path());

    let sidecar_path = out.path().join(format!(
        "perseus/rendered_md/thucydides/histories/book_01/chapter_001_{GRC}.grid_rows.json"
    ));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();

    let expected = chapter_uuid("1", "1", GRC);
    assert_eq!(v["header"]["markdown_uuid"], expected);
    assert!(v["header"]["source_fingerprint"].is_string());
    assert!(v["header"]["render_version"].is_number());

    let row = &v["rows"][0];
    assert_eq!(row["uuid"], expected);
    assert_eq!(row["provider"], "perseus");
    assert_eq!(row["kind"], format!("Chapter ({GRC})"));
    assert_eq!(row["source_label"], "Perseus");
    assert_eq!(row["markdown_uuid"], expected);
    // conversation_name carries the "<b>.<c> <edition-title>" form;
    // with no CTS the title is the short id.
    assert_eq!(row["conversation_name"], "1.1 grc2");
    assert!(row["text"].as_str().unwrap().contains("Θουκυδίδης"));
    assert!(row["qmd_path"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/chapter_001_{GRC}.md")));

    let book_sidecar = out
        .path()
        .join("perseus/rendered_md/thucydides/histories/book_01/index.grid_rows.json");
    let bv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&book_sidecar).unwrap()).unwrap();
    assert_eq!(bv["rows"][0]["uuid"], book_uuid("1"));
    assert_eq!(bv["rows"][0]["kind"], "Book");
}

#[test]
fn chapter_doc_carries_chapter_and_section_rows_sharing_markdown_uuid() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let (_, emitted) = render_fixture(&parsed, out.path());

    // Book 1 Chapter 1 Greek: 2 sections → 1 chapter + 2 section rows.
    let ch11_grc = chapter_uuid("1", "1", GRC);
    let doc = emitted
        .iter()
        .find(|d| d.markdown_uuid == ch11_grc)
        .expect("ch1.1 grc doc not emitted");
    assert_eq!(doc.rows.len(), 3, "expected chapter + 2 section rows");
    assert_eq!(doc.rows[0].kind, format!("Chapter ({GRC})"));
    assert_eq!(doc.rows[1].kind, format!("Section ({GRC})"));
    assert_eq!(doc.rows[2].kind, format!("Section ({GRC})"));
    assert_eq!(
        doc.rows[1].markdown_uuid.as_deref(),
        Some(ch11_grc.as_str())
    );
    assert_eq!(doc.rows[1].uuid, paragraph_uuid("1", "1", "1", GRC));
    assert_eq!(doc.rows[2].uuid, paragraph_uuid("1", "1", "2", GRC));
    assert_eq!(doc.rows[1].message_index, Some(0));
    assert_eq!(doc.rows[2].message_index, Some(1));

    // English side has only section 1.
    let ch11_eng = chapter_uuid("1", "1", ENG);
    let doc_e = emitted
        .iter()
        .find(|d| d.markdown_uuid == ch11_eng)
        .unwrap();
    assert_eq!(doc_e.rows.len(), 2);

    // Each section row's uuid has a matching div anchor in the md.
    let chapter_md = std::fs::read_to_string(&doc.md_path).unwrap();
    for r in doc.rows.iter().skip(1) {
        let needle = format!("data-section-uuid=\"{}\"", r.uuid);
        assert!(
            chapter_md.contains(&needle),
            "{} missing div anchor for section row uuid={}",
            doc.md_path.display(),
            r.uuid
        );
    }
}

#[test]
fn unaligned_corpus_emits_no_edges() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let (_, emitted) = render_fixture(&parsed, out.path());
    // With no alignment pairs configured, every doc is edge-free.
    for d in &emitted {
        assert!(
            d.edges.is_empty(),
            "{} unexpectedly has edges",
            d.markdown_uuid
        );
    }
}

#[test]
fn second_run_is_a_no_op_when_fingerprints_match() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();

    let mut prior: HashMap<String, String> = HashMap::new();
    render::render_all(
        &parsed,
        &PerseusAlignments::default(),
        out.path(),
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |r| {
            prior.insert(r.markdown_uuid.clone(), r.source_fingerprint.clone());
            Ok(())
        },
    )
    .unwrap();

    let summary = render::render_all(
        &parsed,
        &PerseusAlignments::default(),
        out.path(),
        "perseus",
        &Progress::noop(),
        &prior,
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(summary.markdowns_rendered, 0);
    assert_eq!(summary.markdowns_skipped, summary.markdowns_total);
}
