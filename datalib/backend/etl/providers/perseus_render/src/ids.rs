//! Perseus entity ids, keyed by the passage's CTS URN — the id Perseus
//! itself resolves (`urn:cts:greekLit:tlg0003.tlg001.perseus-grc2:1.2.3`);
//! a book index, which no edition owns, takes the work-level form.
//! No stamp anywhere: a classical text has no `created_at` of its own,
//! and the synthetic one the grid sorts by is not the record's.

use datalib_etl_perseus::WORK_URN;
use datalib_id::{composite_key, IdNamespace, Identity, Minter};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Perseus;

pub const KIND_BOOK: &str = "book";
pub const KIND_CHAPTER: &str = "chapter";
pub const KIND_SECTION: &str = "section";
pub const KIND_SENTENCE: &str = "sentence";

const IDS: Minter = Minter::unstamped(ID_NAMESPACE);

pub fn passage_urn(edition: Option<&str>, locator: &str) -> String {
    match edition {
        Some(edition) => format!("{WORK_URN}.{edition}:{locator}"),
        None => format!("{WORK_URN}:{locator}"),
    }
}

pub fn book(source_id: &str, book_n: &str) -> Identity {
    IDS.mint(source_id, KIND_BOOK, passage_urn(None, book_n), None)
}

/// One (book, chapter, edition) — each edition variant gets its own
/// row so the UI can resolve `/api/chat/{uuid}` to a specific
/// edition's markdown. `version` is the edition id (`perseus-grc2`,
/// `1st1K-eng1`, …).
pub fn chapter(source_id: &str, book_n: &str, ch_n: &str, version: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_CHAPTER,
        passage_urn(Some(version), &format!("{book_n}.{ch_n}")),
        None,
    )
}

/// One (book, chapter, section, edition) — the per-paragraph grid rows
/// that deep-link into the chapter doc. The same uuid lands on the
/// `<div data-section-uuid="…">` wrapped around the section in the
/// chapter md, so the UI's lookup matches byte-for-byte and the
/// scroll-and-highlight pane snaps to the section.
pub fn section(source_id: &str, book_n: &str, ch_n: &str, sec_n: &str, version: &str) -> Identity {
    IDS.mint(
        source_id,
        KIND_SECTION,
        passage_urn(Some(version), &format!("{book_n}.{ch_n}.{sec_n}")),
        None,
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
    IDS.mint(
        source_id,
        KIND_SENTENCE,
        composite_key(&[
            &passage_urn(Some(version), &format!("{book_n}.{ch_n}.{sec_n}")),
            &sent_idx.to_string(),
        ]),
        None,
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
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    /// The grid's `upstream_id` is this key, and `perseusView.ts` reads
    /// the locator as what follows the last `:`.
    #[test]
    fn the_key_is_the_cts_passage_urn() {
        assert_eq!(
            book("perseus", "1").natural_key,
            "urn:cts:greekLit:tlg0003.tlg001:1"
        );
        assert_eq!(
            section("perseus", "1", "2", "3", "perseus-grc2").natural_key,
            "urn:cts:greekLit:tlg0003.tlg001.perseus-grc2:1.2.3"
        );
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
