//! Perseus Digital Library provider for [`frankweiler_etl`]: renders
//! the TEI editions of classical works into the shared `rendered_md/`
//! + `.grid_rows.json` tree.
//!
//! Today this is wired for **Thucydides' Histories** (`tlg0003.tlg001`)
//! only — the Greek (`perseus-grc2`) and the English (`1st1K-eng1`)
//! sides aligned by book/chapter/section. Other Perseus editions
//! follow the same TEI shape; when we add a second work we'll move the
//! work-specific constants ([`TLG0003_TLG001`] + `WORK_TITLE`) into a
//! per-work struct on the source config rather than hard-coding.
//!
//! ## Why a provider crate and not a one-off script
//!
//! The corpus itself never changes — Perseus TEI files are
//! version-controlled upstream — so we don't get any benefit from the
//! sync orchestrator's incremental-fetch machinery. We do get:
//!
//!   * **Typed schema coupling.** Grid rows go through the
//!     [`frankweiler_schema::grid_rows::GridRow`] struct, so a column
//!     rename in `schemas/grid_rows.schema.json` breaks the build
//!     instead of silently producing stale sidecars.
//!   * **The same `bazel run //...:sync` UX as every other source.**
//!     Add a `- name: perseus` source (`source: {type: perseus}`) to
//!     `config.yaml` and one command renders + loads + qmd-indexes.
//!   * **A real Bazel test target** ([rust_test
//!     `perseus_translate_test`]) that catches regressions before they
//!     reach a user's data root.
//!
//! ## Configuration
//!
//! ```yaml
//! - name: perseus
//!   source:
//!     type: perseus
//!     sync: {}            # default: Thucydides Histories (grc + eng)
//! ```
//!
//! With a bare `sync: {}` block, `bazel run //frankweiler/backend/sync`
//! downloads the default Thucydides pair from
//! `PerseusDL/canonical-greekLit` (master branch) to
//! `<data_root>/raw/perseus/`, and Translate + Load + qmd-index pick
//! them up on the same run. **No latchkey registration is required**
//! — these URLs are public, so [`extract`] shells out to `curl`
//! directly rather than threading through the shared `latchkey_curl`
//! HTTP path (every other provider uses that path for credential
//! injection — Perseus has nothing to inject).
//!
//! ### Customizing the files list
//!
//! ```yaml
//! - name: perseus
//!   source:
//!     type: perseus
//!     sync:
//!       files:
//!         - tlg0003/tlg001/tlg0003.tlg001.perseus-grc2.xml
//!         - tlg0003/tlg001/tlg0003.tlg001.1st1K-eng1.xml
//! ```
//!
//! Each entry is a subpath under
//! `https://raw.githubusercontent.com/PerseusDL/canonical-greekLit/refs/heads/master/data/`
//! and gets fetched verbatim to `<input_path>/<basename>`. Omit
//! `sync:` entirely to run translate-only against whatever XMLs
//! you've pre-staged at `input_path` (the `ClaudeExport` shape).
//!
//! ### Translate is Thucydides-specific for now
//!
//! The [`crate::render_and_index_md`] path is hardcoded to the Thucydides
//! Histories shape — it looks for the two basenames the default
//! `files` list resolves to. Pointing `files:` at a different work
//! will Extract cleanly but Translate will not find anything to
//! render. Multi-work translate is a follow-up.

use std::sync::OnceLock;

use uuid::Uuid;

pub mod extract;
pub mod processor;
pub mod render_and_index_md;

/// Frozen UUIDv5 seed string. Despite the name carrying `perseus-grc2`,
/// this is just a stable namespace prefix for *every* row PK in this
/// crate — it predates multi-edition support and is kept verbatim so
/// `book_uuid` stays byte-for-byte stable against existing data roots.
/// Per-edition PKs append the edition id (see [`chapter_uuid`]).
pub const TLG0003_TLG001: &str = "urn:cts:greekLit:tlg0003.tlg001.perseus-grc2";

/// CTS work URN (no edition suffix). `__cts__.xml` edition `urn`s are
/// `<this>.<edition-id>`, which [`crate::render_and_index_md::parse`] strips to
/// recover the edition id.
pub const WORK_URN: &str = "urn:cts:greekLit:tlg0003.tlg001";

/// Filename prefix every edition TEI shares:
/// `tlg0003.tlg001.<edition-id>.xml`. The parser strips this (and the
/// `.xml` suffix) to recover the edition id.
pub const TLG_FILE_PREFIX: &str = "tlg0003.tlg001.";

/// The CTS work URN, used by the parser to strip edition `urn`s.
pub fn cts_urn() -> &'static str {
    WORK_URN
}

/// Displayed in the grid's `project` column.
pub const WORK_TITLE: &str = "Thucydides, Histories";

/// Short label embedded in grid `kind` strings + chapter titles.
pub const WORK_SHORT: &str = "Thucydides";

/// Stable namespace for every UUIDv5 derivation in this crate. Frozen
/// to the value the original `tools/perseus_ingest/ingest_thucydides.py`
/// script used, so re-running the Rust path against an existing data
/// root is a no-op (PKs match byte-for-byte).
pub fn perseus_uuid_ns() -> &'static Uuid {
    static NS: OnceLock<Uuid> = OnceLock::new();
    NS.get_or_init(|| {
        Uuid::parse_str("a1f5d2c4-8e1f-4bd1-9bc8-9c5d3a6e7b21").expect("valid perseus uuid ns")
    })
}

/// PK for one Thucydides book.
pub fn book_uuid(book_n: &str) -> String {
    let name = format!("{TLG0003_TLG001}:book{book_n}");
    Uuid::new_v5(perseus_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// PK for one (book, chapter, edition) — each edition variant gets its
/// own row so the UI can resolve `/api/chat/{uuid}` to a specific
/// edition's markdown. `version` is the edition id (`perseus-grc2`,
/// `1st1K-eng1`, …).
pub fn chapter_uuid(book_n: &str, ch_n: &str, version: &str) -> String {
    let name = format!("{TLG0003_TLG001}:book{book_n}:ch{ch_n}:{version}");
    Uuid::new_v5(perseus_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// PK for one (book, chapter, section, language) — drives the
/// per-paragraph grid rows that deep-link into the chapter doc. The
/// same uuid lands on the `<div data-section-uuid="…">` wrapped
/// around the section in the chapter md, so the UI's
/// `querySelector('[data-section-uuid="…"]')` lookup matches
/// byte-for-byte and the scroll-and-highlight pane snaps to the
/// section. See `frankweiler/ui/src/components/ChatBody.vue`'s
/// `applySelection` for the matching code.
pub fn paragraph_uuid(book_n: &str, ch_n: &str, sec_n: &str, version: &str) -> String {
    let name = format!("{TLG0003_TLG001}:book{book_n}:ch{ch_n}:sec{sec_n}:{version}");
    Uuid::new_v5(perseus_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Anchor UUID for one sentence within a section. The renderer wraps
/// each sentence (split by [`crate::render_and_index_md::align::split`]) in its
/// own `<span data-section-uuid="…">` using this UUID; the
/// bilingual-alignment `edges` rows reference these as
/// `src_anchor_uuid` / `dst_anchor_uuid` so the UI can highlight the
/// aligned sentence on the other-language side when one is clicked.
///
/// `sent_idx` is 0-based, matching the order the splitter emits.
/// Derivation includes the sentence index so a section with N
/// sentences gets N distinct anchor UUIDs; reordering / re-splitting
/// the same section would shift them, which is exactly what we want
/// — the alignment edges would have to be re-derived alongside.
pub fn paragraph_sentence_uuid(
    book_n: &str,
    ch_n: &str,
    sec_n: &str,
    version: &str,
    sent_idx: usize,
) -> String {
    let name =
        format!("{TLG0003_TLG001}:book{book_n}:ch{ch_n}:sec{sec_n}:{version}:sent{sent_idx}");
    Uuid::new_v5(perseus_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Stable identifier for one `edges` row. Producers SHOULD derive
/// edge UUIDs deterministically so re-ingest replaces existing rows
/// rather than inserting duplicates. The canonical input is the
/// directed tuple (src_markdown, src_anchor, dst_markdown,
/// dst_anchor, label) — same fields the schema's `x-primary-key`
/// section spells out.
pub fn edge_uuid(
    src_md: &str,
    src_anchor: Option<&str>,
    dst_md: &str,
    dst_anchor: Option<&str>,
    label: Option<&str>,
) -> String {
    let name = format!(
        "edge:{src_md}/{}->{dst_md}/{}|{}",
        src_anchor.unwrap_or(""),
        dst_anchor.unwrap_or(""),
        label.unwrap_or(""),
    );
    Uuid::new_v5(perseus_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the derived UUIDs against the values produced by the
    /// original Python script. If we ever change the namespace or the
    /// derivation string, an existing data root's PKs would all change
    /// silently — and the loader would insert dupes next to the old
    /// rows instead of updating them. Catch that here.
    #[test]
    fn uuids_match_legacy_python_ingest() {
        assert_eq!(
            book_uuid("1"),
            // python3 -c "import uuid; print(uuid.uuid5(uuid.UUID('a1f5d2c4-8e1f-4bd1-9bc8-9c5d3a6e7b21'), 'urn:cts:greekLit:tlg0003.tlg001.perseus-grc2:book1'))"
            "186e8a85-1d9f-56fd-8d47-2a6dd33f2f13"
        );
        // Each edition gets a distinct chapter PK (the edition id is
        // part of the derivation string).
        assert_ne!(
            chapter_uuid("1", "1", "perseus-grc2"),
            chapter_uuid("1", "1", "1st1K-eng1"),
        );
    }
}
