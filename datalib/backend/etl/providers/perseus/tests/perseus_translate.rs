//! End-to-end render test against the checked-in tiny TEI fixture.
//! Feeds two TEI editions (Greek + English) through `parse + render_all`
//! and asserts what it renders — the documents, emitted rows
//! structure, key text fragments. This is the regression net for the
//! multi-edition render path: if the parser silently drops a section
//! or the renderer flips a UUID derivation, this fails before bad data
//! hits a user's root.

use std::path::PathBuf;

use datalib_etl::progress::Progress;
use datalib_etl_perseus::TLG_FILE_PREFIX;

fn book_uuid(b: &str) -> String {
    datalib_etl_perseus_render::ids::book("perseus", b).uuid
}

fn chapter_uuid(b: &str, c: &str, e: &str) -> String {
    datalib_etl_perseus_render::ids::chapter("perseus", b, c, e).uuid
}

fn paragraph_uuid(b: &str, c: &str, s: &str, e: &str) -> String {
    datalib_etl_perseus_render::ids::section("perseus", b, c, s, e).uuid
}
use datalib_etl_perseus_render::render::align::PerseusAlignments;
use datalib_etl_perseus_render::render::{parse, render};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::section::join;

const GRC: &str = "perseus-grc2";
const ENG: &str = "1st1K-eng1";

fn fixture_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PERSEUS_FIXTURE_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/perseus_tiny")
}

fn render_fixture(parsed: &parse::ParsedPerseus) -> (render::RenderSummary, Vec<RenderedMarkdown>) {
    let mut emitted: Vec<RenderedMarkdown> = Vec::new();
    let summary = render::render_all(
        parsed,
        &PerseusAlignments::default(),
        "perseus",
        &Progress::noop(),
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
    let parsed = parse::parse(&fixture_dir()).expect("parse tiny fixture");

    // 2 editions; grc sorts first.
    assert_eq!(parsed.editions.len(), 2);
    assert_eq!(parsed.editions[0].id, GRC);
    assert_eq!(parsed.editions[1].id, ENG);
    // 2 books × 2 chapters (union of both editions).
    assert_eq!(parsed.books.len(), 2);
    assert_eq!(parsed.books[0].chapters.len(), 2);
    assert_eq!(parsed.books[1].chapters.len(), 2);

    let (summary, emitted) = render_fixture(&parsed);

    // 2 books + 4 chapters × 2 editions = 10 docs.
    assert_eq!(summary.markdowns_total, 10);
    assert_eq!(summary.markdowns_rendered, 10);
    assert_eq!(emitted.len(), 10);
    // One bucket for the whole tree, so the driver's sweep of what a
    // run did not emit reaches every document; and the files it names
    // are what the bucket declares as its inputs.
    let bucket = render::bucket_key("perseus");
    assert!(emitted
        .iter()
        .all(|d| d.bucket_key.as_deref() == Some(bucket.as_str())));
    assert_eq!(
        parsed.files,
        [
            format!("{TLG_FILE_PREFIX}{ENG}.xml"),
            format!("{TLG_FILE_PREFIX}{GRC}.xml"),
        ]
    );

    let page = |uuid: String| {
        let doc = emitted
            .iter()
            .find(|d| d.markdown_uuid == uuid)
            .unwrap_or_else(|| panic!("no document {uuid}"));
        join(&doc.sections)
    };
    page(book_uuid("1"));

    // Book 1 chapter 1, Greek side has both sections. Unaligned
    // editions render section text verbatim (no per-sentence spans).
    let body = page(chapter_uuid("1", "1", GRC));
    assert!(body.contains("Θουκυδίδης Ἀθηναῖος ξυνέγραψε."));
    assert!(body.contains("### 1.1.1"));
    assert!(body.contains("### 1.1.2"));
    assert!(!body.contains("<span data-section-uuid"));

    // English side: section 2 is the deliberate translation gap.
    let body_eng = page(chapter_uuid("1", "1", ENG));
    assert!(body_eng.contains("the Athenian wrote"));
    assert!(body_eng.contains("### 1.1.1"));
    assert!(!body_eng.contains("### 1.1.2"));

    // Book 2 chapter 2: bare chapter text → section "1".
    let body22 = page(chapter_uuid("2", "2", GRC));
    assert!(body22.contains("### 2.2.1"));
}

#[test]
fn documents_carry_stable_uuids_and_provider_metadata() {
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let (_summary, emitted) = render_fixture(&parsed);

    let expected = chapter_uuid("1", "1", GRC);
    let chapter = emitted
        .iter()
        .find(|d| d.markdown_uuid == expected)
        .expect("the grc chapter was emitted");
    assert!(chapter.render_version > 0);

    let row = &chapter.rows[0];
    assert_eq!(row.uuid, expected);
    assert_eq!(row.provider, "perseus");
    assert_eq!(row.kind, format!("Chapter ({GRC})"));
    assert_eq!(row.source_label, "Perseus");
    assert_eq!(row.markdown_uuid.as_deref(), Some(expected.as_str()));
    // conversation_name carries the "<b>.<c> <edition-title>" form;
    // with no CTS the title is the short id.
    assert_eq!(row.conversation_name.as_deref(), Some("1.1 grc2"));
    assert!(row.preview.contains("Θουκυδίδης"));

    let book = emitted
        .iter()
        .find(|d| d.markdown_uuid == book_uuid("1"))
        .expect("the book index was emitted");
    assert_eq!(book.rows[0].uuid, book_uuid("1"));
    assert_eq!(book.rows[0].kind, "Book");
}

#[test]
fn chapter_doc_carries_chapter_and_section_rows_sharing_markdown_uuid() {
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let (_, emitted) = render_fixture(&parsed);

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
    let chapter_md = join(&doc.sections);
    for r in doc.rows.iter().skip(1) {
        let needle = format!("data-section-uuid=\"{}\"", r.uuid);
        assert!(
            chapter_md.contains(&needle),
            "{} missing div anchor for section row uuid={}",
            doc.markdown_uuid,
            r.uuid
        );
    }
}

#[test]
fn unaligned_corpus_emits_no_edges() {
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let (_, emitted) = render_fixture(&parsed);
    // With no alignment pairs configured, every doc is edge-free.
    for d in &emitted {
        assert!(
            d.edges.is_empty(),
            "{} unexpectedly has edges",
            d.markdown_uuid
        );
    }
}
