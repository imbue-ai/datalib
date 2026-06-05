//! End-to-end translate test against the checked-in tiny TEI fixture.
//! Feeds two TEI XMLs (Greek + English) through `parse + render_all`
//! and asserts the rendered tree shape — file paths, sidecar JSON
//! structure, key text fragments. This is the regression net for the
//! Rust port of `tools/perseus_ingest/ingest_thucydides.py`: if the
//! parser silently drops a section or the renderer flips a UUID
//! derivation, this test fails before bad data hits a user's root.

use std::collections::HashMap;
use std::path::PathBuf;

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_perseus::translate::{parse, render};
use frankweiler_etl_perseus::{book_uuid, chapter_uuid, paragraph_uuid};

/// Resolves the fixture dir the same way the other provider tests do:
/// Bazel sets `PERSEUS_FIXTURE_DIR` via `env`, Cargo falls back to the
/// path under `CARGO_MANIFEST_DIR`.
fn fixture_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PERSEUS_FIXTURE_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/perseus_tiny")
}

#[test]
fn renders_all_books_chapters_and_languages() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).expect("parse tiny fixture");

    // Sanity: 2 books × 2 chapters × varying sections.
    assert_eq!(parsed.books.len(), 2);
    assert_eq!(parsed.books[0].chapters.len(), 2);
    assert_eq!(parsed.books[1].chapters.len(), 2);

    let mut emitted: Vec<RenderedMarkdown> = Vec::new();
    let summary = render::render_all(
        &parsed,
        out.path(),
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |r: RenderedMarkdown| {
            emitted.push(r);
            Ok(())
        },
    )
    .expect("render");

    // 2 books + 2 × 4 chapter docs (4 chapters × 2 langs) = 10 docs.
    assert_eq!(summary.markdowns_total, 10);
    assert_eq!(summary.markdowns_rendered, 10);
    assert_eq!(summary.markdowns_skipped, 0);
    assert_eq!(emitted.len(), 10);

    // Book 1 index landed at the expected path.
    let book1 = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_01/index.md");
    assert!(book1.exists(), "missing {}", book1.display());

    // Book 1 chapter 1, Greek side has both sections.
    let ch11_grc = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_grc.md");
    let body = std::fs::read_to_string(&ch11_grc).unwrap();
    // The renderer wraps each section's first word in an inline
    // `<span data-section-uuid="…">…</span>` for the bilingual-
    // alignment edge anchor, so the section text is split across the
    // span boundary in the rendered markdown — assert on the tail
    // after the first word instead of the whole sentence.
    assert!(body.contains("Ἀθηναῖος ξυνέγραψε."));
    assert!(body.contains("πόλεμον τῶν Πελοποννησίων."));
    // Header refs read "B.C.S".
    assert!(body.contains("### 1.1.1"));
    assert!(body.contains("### 1.1.2"));

    // English side, same chapter: section 2 is missing on the English
    // edition by design (translation gap). The renderer skips empty
    // text; ref header for section 2 must NOT appear.
    let ch11_eng = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_eng.md");
    let body_eng = std::fs::read_to_string(&ch11_eng).unwrap();
    assert!(body_eng.contains("the Athenian wrote"));
    assert!(body_eng.contains("### 1.1.1"));
    assert!(!body_eng.contains("### 1.1.2"));

    // Book 2 chapter 2: chapter div carries text directly (no section
    // children). Parser collapses that to section "1"; renderer emits
    // a single section header.
    let ch22_grc = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_02/chapter_002_grc.md");
    let body22 = std::fs::read_to_string(&ch22_grc).unwrap();
    assert!(body22.contains("### 2.2.1"));
    // "Κεφαλὴ" is the first word and lives inside the inline span;
    // "χωρίς" is the tail outside the span. See the matching
    // assertion in `renders_all_books_chapters_and_languages` for
    // the chapter 1 case.
    assert!(body22.contains("χωρίς"));
}

#[test]
fn sidecars_carry_stable_uuids_and_provider_metadata() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();
    render::render_all(
        &parsed,
        out.path(),
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |_| Ok(()),
    )
    .unwrap();

    let sidecar_path = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_grc.grid_rows.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();

    // Header keys the loader needs.
    let expected = chapter_uuid("1", "1", "grc");
    assert_eq!(v["header"]["markdown_uuid"], expected);
    assert!(v["header"]["source_fingerprint"].is_string());
    assert!(v["header"]["render_version"].is_number());

    // Row shape: keys the grid + qmd index depend on.
    let row = &v["rows"][0];
    assert_eq!(row["uuid"], expected);
    assert_eq!(row["provider"], "perseus");
    assert_eq!(row["kind"], "Chapter (grc)");
    assert_eq!(row["source_label"], "Perseus");
    assert_eq!(row["conversation_uuid"], expected);
    assert_eq!(row["markdown_uuid"], expected);
    assert!(row["text"].as_str().unwrap().contains("Θουκυδίδης"));
    assert!(row["qmd_path"]
        .as_str()
        .unwrap()
        .ends_with("/chapter_001_grc.md"));

    // Book sidecar uses the book uuid, not the chapter uuid.
    let book_sidecar = out
        .path()
        .join("rendered_md/perseus/thucydides/histories/book_01/index.grid_rows.json");
    let bv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&book_sidecar).unwrap()).unwrap();
    assert_eq!(bv["rows"][0]["uuid"], book_uuid("1"));
    assert_eq!(bv["rows"][0]["kind"], "Book");
}

#[test]
fn chapter_doc_carries_chapter_and_section_rows_sharing_markdown_uuid() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();

    let mut emitted: Vec<RenderedMarkdown> = Vec::new();
    render::render_all(
        &parsed,
        out.path(),
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |r| {
            emitted.push(r);
            Ok(())
        },
    )
    .unwrap();

    // Find Book 1 Chapter 1 Greek doc. The fixture has 2 grc
    // sections under that chapter (sec 1, sec 2), so we expect
    // 1 chapter row + 2 section rows.
    let ch11_grc = chapter_uuid("1", "1", "grc");
    let doc = emitted
        .iter()
        .find(|d| d.markdown_uuid == ch11_grc)
        .expect("ch1.1 grc doc not emitted");
    assert_eq!(doc.rows.len(), 3, "expected chapter + 2 section rows");
    assert_eq!(doc.rows[0].kind, "Chapter (grc)");
    assert_eq!(doc.rows[1].kind, "Section (grc)");
    assert_eq!(doc.rows[2].kind, "Section (grc)");
    // Section rows point at the chapter doc.
    assert_eq!(
        doc.rows[1].markdown_uuid.as_deref(),
        Some(ch11_grc.as_str())
    );
    assert_eq!(doc.rows[1].uuid, paragraph_uuid("1", "1", "1", "grc"));
    assert_eq!(doc.rows[2].uuid, paragraph_uuid("1", "1", "2", "grc"));
    // message_index counts non-empty sections in emission order,
    // starting at 0 within each chapter — matches the SPA's `#m{idx}`
    // fragment scheme.
    assert_eq!(doc.rows[1].message_index, Some(0));
    assert_eq!(doc.rows[2].message_index, Some(1));

    // English side has only section 1 (sec 2 is the translation gap
    // we deliberately put in the fixture). So 1 chapter + 1 section
    // = 2 rows.
    let ch11_eng = chapter_uuid("1", "1", "eng");
    let doc_e = emitted
        .iter()
        .find(|d| d.markdown_uuid == ch11_eng)
        .unwrap();
    assert_eq!(doc_e.rows.len(), 2);

    // The chapter md actually carries matching div anchors for each
    // section row's uuid — without this the SPA's deep-link
    // querySelector would 404 and the click would just open the doc
    // unscrolled. Read the file and grep for the attribute.
    let chapter_md = std::fs::read_to_string(
        out.path()
            .join(doc.md_path.strip_prefix(out.path()).unwrap()),
    )
    .unwrap();
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
fn book_index_emits_chapter_edges_to_every_chapter_lang_pair() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();
    let mut emitted: Vec<frankweiler_etl::load::RenderedMarkdown> = Vec::new();
    render::render_all(
        &parsed,
        out.path(),
        "perseus",
        &Progress::noop(),
        &HashMap::new(),
        &mut |r| {
            emitted.push(r);
            Ok(())
        },
    )
    .unwrap();

    // The book doc is now a pure navigation entry — its rendered
    // body is empty (no `## Chapters` table) and the chapter cross-
    // links live in `edges` instead.
    let idx = std::fs::read_to_string(
        out.path()
            .join("rendered_md/perseus/thucydides/histories/book_01/index.md"),
    )
    .unwrap();
    assert!(!idx.contains("## Chapters"));
    assert!(!idx.contains("/#/chat/"));

    // Edges: one whole-doc edge per (chapter, language) pair,
    // labeled "chapter". Fixture's book 1 has 2 chapters → 4 edges.
    let bk = emitted
        .iter()
        .find(|d| d.markdown_uuid == book_uuid("1"))
        .expect("book doc emitted");
    assert_eq!(bk.edges.len(), 4);
    for e in &bk.edges {
        assert_eq!(e.label.as_deref(), Some("chapter"));
    }
    for (b, c) in [("1", "1"), ("1", "2")] {
        for lang in ["grc", "eng"] {
            let dst = chapter_uuid(b, c, lang);
            assert!(
                bk.edges.iter().any(|e| e.dst_markdown_uuid == dst),
                "missing edge to {b}.{c} {lang}"
            );
        }
    }
}

#[test]
fn second_run_is_a_no_op_when_fingerprints_match() {
    let out = tempfile::tempdir().unwrap();
    let parsed = parse::parse(&fixture_dir()).unwrap();

    // First pass: capture fingerprints the orchestrator would store.
    let mut prior: HashMap<String, String> = HashMap::new();
    render::render_all(
        &parsed,
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

    // Second pass with those fingerprints — every doc skips.
    let summary = render::render_all(
        &parsed,
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
