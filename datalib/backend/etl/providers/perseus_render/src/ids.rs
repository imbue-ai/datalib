//! Perseus entity ids: content-scoped over the CTS locator and edition.
//! No stamp anywhere: a classical text has no `created_at` of its own,
//! and the synthetic one the grid sorts by is not the record's.

use datalib_etl_perseus::WORK_URN;
use datalib_id::{composite_key, IdNamespace, Identity, Scope};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Perseus;

pub const KIND_BOOK: &str = "book";
pub const KIND_CHAPTER: &str = "chapter";
pub const KIND_SECTION: &str = "section";
pub const KIND_SENTENCE: &str = "sentence";

fn identity(source_id: &str, entity_kind: &'static str, natural_key: String) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Scope::Content,
        entity_kind,
        natural_key,
        None,
    )
}

/// `1` — the book number under [`WORK_URN`].
pub fn book(source_id: &str, book_n: &str) -> Identity {
    identity(source_id, KIND_BOOK, composite_key(&[WORK_URN, book_n]))
}

/// One (book, chapter, edition) — each edition variant gets its own
/// row so the UI can resolve `/api/chat/{uuid}` to a specific
/// edition's markdown. `version` is the edition id (`perseus-grc2`,
/// `1st1K-eng1`, …).
pub fn chapter(source_id: &str, book_n: &str, ch_n: &str, version: &str) -> Identity {
    identity(
        source_id,
        KIND_CHAPTER,
        composite_key(&[WORK_URN, &format!("{book_n}.{ch_n}"), version]),
    )
}

/// One (book, chapter, section, edition) — the per-paragraph grid rows
/// that deep-link into the chapter doc. The same uuid lands on the
/// `<div data-section-uuid="…">` wrapped around the section in the
/// chapter md, so the UI's lookup matches byte-for-byte and the
/// scroll-and-highlight pane snaps to the section.
pub fn section(source_id: &str, book_n: &str, ch_n: &str, sec_n: &str, version: &str) -> Identity {
    identity(
        source_id,
        KIND_SECTION,
        composite_key(&[WORK_URN, &format!("{book_n}.{ch_n}.{sec_n}"), version]),
    )
}

/// The anchor for one sentence within a section. The renderer wraps
/// each sentence in its own `<span data-section-uuid="…">` using this,
/// and the bilingual-alignment `edges` rows reference it as
/// `src_anchor_uuid` / `dst_anchor_uuid` so the UI can highlight the
/// aligned sentence on the other-language side when one is clicked.
pub fn sentence(
    source_id: &str,
    book_n: &str,
    ch_n: &str,
    sec_n: &str,
    version: &str,
    sent_idx: usize,
) -> Identity {
    identity(
        source_id,
        KIND_SENTENCE,
        composite_key(&[
            WORK_URN,
            &format!("{book_n}.{ch_n}.{sec_n}"),
            version,
            &sent_idx.to_string(),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            book("perseus", "1"),
            chapter("perseus", "1", "1", "perseus-grc2"),
            section("perseus", "1", "1", "1", "perseus-grc2"),
            sentence("perseus", "1", "1", "1", "perseus-grc2", 3),
        ] {
            assert_eq!(
                got.uuid,
                datalib_id::entity_id_str(
                    ID_NAMESPACE,
                    "perseus",
                    Scope::Content,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    /// Each edition gets a distinct chapter id: the edition id is part
    /// of the key.
    #[test]
    fn editions_and_kinds_separate() {
        assert_ne!(
            chapter("perseus", "1", "1", "perseus-grc2").uuid,
            chapter("perseus", "1", "1", "1st1K-eng1").uuid,
        );
        assert_ne!(
            book("perseus", "1").uuid,
            chapter("perseus", "1", "1", "x").uuid
        );
    }
}
