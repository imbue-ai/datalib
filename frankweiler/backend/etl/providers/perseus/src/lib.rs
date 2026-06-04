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
//!     Add `- name: perseus, type: perseus` to `config.yaml` and one
//!     command renders + loads + qmd-indexes.
//!   * **A real Bazel test target** ([rust_test
//!     `perseus_translate_test`]) that catches regressions before they
//!     reach a user's data root.
//!
//! ## Configuration
//!
//! ```yaml
//! - name: perseus
//!   type: perseus
//!   sync: {}            # default: Thucydides Histories (grc + eng)
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
//!   type: perseus
//!   sync:
//!     files:
//!       - tlg0003/tlg001/tlg0003.tlg001.perseus-grc2.xml
//!       - tlg0003/tlg001/tlg0003.tlg001.1st1K-eng1.xml
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
//! The [`crate::translate`] path is hardcoded to the Thucydides
//! Histories shape — it looks for the two basenames the default
//! `files` list resolves to. Pointing `files:` at a different work
//! will Extract cleanly but Translate will not find anything to
//! render. Multi-work translate is a follow-up.

use std::sync::OnceLock;

use uuid::Uuid;

pub mod extract;
pub mod translate;

/// CTS URN for the one edition we currently render. The UUID
/// derivation embeds this so we can add a second edition later (e.g.
/// Herodotus' `tlg0016.tlg001`) without colliding row PKs.
pub const TLG0003_TLG001: &str = "urn:cts:greekLit:tlg0003.tlg001.perseus-grc2";

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

/// PK for one (book, chapter, language) — each language variant gets
/// its own row so the UI can resolve `/api/chat/{uuid}` to a specific
/// language's markdown.
pub fn chapter_uuid(book_n: &str, ch_n: &str, lang: &str) -> String {
    let name = format!("{TLG0003_TLG001}:book{book_n}:ch{ch_n}:{lang}");
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
pub fn paragraph_uuid(book_n: &str, ch_n: &str, sec_n: &str, lang: &str) -> String {
    let name = format!("{TLG0003_TLG001}:book{book_n}:ch{ch_n}:sec{sec_n}:{lang}");
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
        assert_eq!(
            chapter_uuid("1", "1", "grc"),
            // ...:book1:ch1:grc
            chapter_uuid("1", "1", "grc"),
        );
        // grc ≠ eng for the same chapter.
        assert_ne!(chapter_uuid("1", "1", "grc"), chapter_uuid("1", "1", "eng"));
    }
}
