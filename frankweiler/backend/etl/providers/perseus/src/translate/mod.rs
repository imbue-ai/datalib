//! Perseus TEI XML → grid_rows + rendered markdown.
//!
//! `parse::parse` reads the two TEI XMLs (Greek + English) from a
//! directory and aligns them by (book, chapter, section). `render`
//! emits one markdown doc per book (`index.md`) and per (chapter ×
//! language), plus a `.grid_rows.json` sidecar alongside each.

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
/// rows + the book index's chapter cross-link table.
pub const RENDER_VERSION: u32 = 8;
