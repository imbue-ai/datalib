//! Perseus TEI XML → grid_rows + rendered markdown.
//!
//! `parse::parse` reads the two TEI XMLs (Greek + English) from a
//! directory and aligns them by (book, chapter, section). `render`
//! emits one markdown doc per book (`index.md`) and per (chapter ×
//! language), plus a `.grid_rows.json` sidecar alongside each.

pub mod align;
pub mod parse;
pub mod render;

/// The two TEI filenames we expect under `input_path`. These are the
/// canonical Perseus filenames — both are tracked under PerseusDL's
/// `canonical-greekLit` repo at:
/// <https://github.com/PerseusDL/canonical-greekLit/tree/master/data/tlg0003/tlg001>
pub const GRC_FILENAME: &str = "tlg0003.tlg001.perseus-grc2.xml";
pub const ENG_FILENAME: &str = "tlg0003.tlg001.1st1K-eng1.xml";

/// Bump when the rendered markdown layout or grid row shape changes
/// enough that every existing doc needs re-rendering. v7 was the
/// initial Rust port (same row shape as the Python `v6`, just to
/// flip the fingerprint once). v8 adds the per-section `<div
/// data-section-uuid="…">` deep-link wrappers + per-paragraph grid
/// rows + the book index's chapter cross-link table. v9 drops the
/// inline `*Other:*` cross-language hyperlink (now expressed as an
/// `edges` row with no markdown footprint) and wraps each section's
/// first word in a `<span data-section-uuid="…">` that the
/// bilingual-alignment edges hang off. v10 replaces the doc-level
/// edge's "cross-language" label with the destination language
/// itself ("Greek" / "English") so the UI's outgoing-destinations
/// list reads as a human would expect. v11 puts the language into
/// the chapter / section rows' `conversation_name` (and therefore
/// the canonical `markdowns.title`) so "Thucydides 1.1 (Greek)" vs.
/// "Thucydides 1.1 (English)" stays distinguishable everywhere the
/// title surfaces, not just in `kind`. v12 reduces the book doc to
/// a pure navigation entry — empty body, no `## Chapters` table —
/// and replaces the inline chapter cross-links with one `chapter`-
/// labeled outgoing edge per (chapter, language) pair. v13 upgrades
/// the bilingual-alignment edges from section-level placeholders
/// (first-word ↔ first-word) to per-sentence anchors: each section's
/// body now contains one `<span data-section-uuid="…">` per sentence
/// (UUIDs derived via `paragraph_sentence_uuid`), and the edges
/// table carries one row per aligned (grc-sentence, eng-sentence)
/// pair — within-section sentence alignment is computed by the
/// `translate::align` module using Ancient-Greek-BERT.
pub const RENDER_VERSION: u32 = 13;
