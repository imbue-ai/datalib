//! Within-section sentence-level bilingual alignment.
//!
//! The Perseus CTS spine already gives us section-level alignment
//! for free (`1.4.1` exists in both editions). What this module
//! adds is the next layer: when the translator split one Greek
//! sentence into 2-3 English ones (or vice versa), find which goes
//! with which. About 50% of Thucydides' sections need this.
//!
//! Pipeline per section:
//!   1. Split grc and eng into sentences with [`split`].
//!   2. If both sides have ≤1 sentence, return the trivial 1:1 (no
//!      model call needed). This is ~50% of sections — keeps cost
//!      down.
//!   3. Embed each sentence on each side via [`embed::Embedder`]
//!      (mean-pooled Ancient-Greek-BERT).
//!   4. Run [`dp::align`] for the Bertalign-style sentence grouping.
//!
//! The result is a `Vec<SentenceGroup>` per section, each carrying
//! which grc-sentence-indices group with which eng-sentence-indices
//! plus the underlying sentence text + byte spans (so the renderer
//! can wrap the right substrings in anchor spans).
//!
//! Errors are surfaced — translate will refuse to render if the
//! aligner errors out, rather than silently falling back to the
//! pre-existing section-level placeholder edges. This is a build
//! step, not a serving step; failing loudly is the right default.

pub mod dp;
pub mod embed;
pub mod split;

use std::collections::HashMap;

use anyhow::Result;

use crate::translate::parse::Section;

pub use embed::Embedder;
pub use split::Sentence;

/// One aligned grouping inside a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceGroup {
    /// 0-indexed positions within `grc_sentences` covered by this group.
    pub grc_indices: Vec<usize>,
    /// 0-indexed positions within `eng_sentences` covered by this group.
    pub eng_indices: Vec<usize>,
}

/// Aligned sentence layout for one section.
#[derive(Debug, Clone)]
pub struct SectionAlignment {
    pub grc_sentences: Vec<Sentence>,
    pub eng_sentences: Vec<Sentence>,
    pub groups: Vec<SentenceGroup>,
}

impl SectionAlignment {
    /// Trivial single-group alignment covering both sides 1:1 (used
    /// when neither side has more than one sentence — no model call).
    pub fn trivial(grc: Vec<Sentence>, eng: Vec<Sentence>) -> Self {
        let grc_indices: Vec<usize> = (0..grc.len()).collect();
        let eng_indices: Vec<usize> = (0..eng.len()).collect();
        let groups = if grc_indices.is_empty() && eng_indices.is_empty() {
            Vec::new()
        } else {
            vec![SentenceGroup {
                grc_indices,
                eng_indices,
            }]
        };
        Self {
            grc_sentences: grc,
            eng_sentences: eng,
            groups,
        }
    }
}

/// Align one section. Calls the embedder only when at least one side
/// has more than one sentence — the common trivial case skips the
/// model entirely.
pub fn align_section(emb: &Embedder, section: &Section) -> Result<SectionAlignment> {
    let grc = split::split_grc(&section.grc);
    let eng = split::split_eng(&section.eng);

    if grc.len() <= 1 && eng.len() <= 1 {
        return Ok(SectionAlignment::trivial(grc, eng));
    }
    if grc.is_empty() || eng.is_empty() {
        return Ok(SectionAlignment::trivial(grc, eng));
    }

    let grc_emb: Vec<Vec<f32>> = grc
        .iter()
        .map(|s| emb.embed_one(&s.text))
        .collect::<Result<_>>()?;
    let eng_emb: Vec<Vec<f32>> = eng
        .iter()
        .map(|s| emb.embed_one(&s.text))
        .collect::<Result<_>>()?;
    let grc_lens: Vec<usize> = grc.iter().map(|s| s.text.chars().count()).collect();
    let eng_lens: Vec<usize> = eng.iter().map(|s| s.text.chars().count()).collect();
    let groups = dp::align(&grc_emb, &eng_emb, &grc_lens, &eng_lens);
    let groups = groups
        .into_iter()
        .map(|g| SentenceGroup {
            grc_indices: g.grc,
            eng_indices: g.eng,
        })
        .collect();
    Ok(SectionAlignment {
        grc_sentences: grc,
        eng_sentences: eng,
        groups,
    })
}

/// Per-section sentence alignments, keyed by `(book_n, ch_n, sec_n)`.
/// Output of [`align_all`]; consumed by `render_all`.
#[derive(Debug, Default)]
pub struct PerseusAlignments {
    by_section: HashMap<(String, String, String), SectionAlignment>,
}

impl PerseusAlignments {
    pub fn from_map(map: HashMap<(String, String, String), SectionAlignment>) -> Self {
        Self { by_section: map }
    }

    pub fn get(&self, book_n: &str, ch_n: &str, sec_n: &str) -> Option<&SectionAlignment> {
        self.by_section
            .get(&(book_n.to_string(), ch_n.to_string(), sec_n.to_string()))
    }

    /// Resolve the alignment for a section, falling back to the
    /// trivial 1:1 grouping. Used by the renderer so a fixture-based
    /// test (which doesn't run the model) still produces a sensible
    /// output even though `by_section` is empty.
    pub fn get_or_trivial(
        &self,
        book_n: &str,
        ch_n: &str,
        sec_n: &str,
        sec: &Section,
    ) -> SectionAlignment {
        if let Some(a) = self.get(book_n, ch_n, sec_n) {
            return a.clone();
        }
        SectionAlignment::trivial(split::split_grc(&sec.grc), split::split_eng(&sec.eng))
    }
}

/// Align every section in a parsed work. Builds the embedder once
/// up-front (one model load) iff any section needs it. Embedder
/// failures short-circuit; per-section failures bubble up.
pub async fn align_all(
    parsed: &crate::translate::parse::ParsedPerseus,
) -> Result<PerseusAlignments> {
    let n_nontrivial: usize = parsed
        .books
        .iter()
        .flat_map(|b| b.chapters.iter())
        .flat_map(|c| c.sections.iter())
        .filter(|s| needs_model(s))
        .count();
    tracing::info!(
        "perseus alignment: {} sections need model inference",
        n_nontrivial
    );

    let emb = if n_nontrivial > 0 {
        Some(Embedder::load().await?)
    } else {
        None
    };

    let mut by_section: HashMap<(String, String, String), SectionAlignment> = HashMap::new();
    for book in &parsed.books {
        for chapter in &book.chapters {
            for sec in &chapter.sections {
                let alignment = if needs_model(sec) {
                    align_section(
                        emb.as_ref().expect("loaded above when n_nontrivial>0"),
                        sec,
                    )?
                } else {
                    SectionAlignment::trivial(
                        split::split_grc(&sec.grc),
                        split::split_eng(&sec.eng),
                    )
                };
                by_section.insert(
                    (book.n.clone(), chapter.n.clone(), sec.n.clone()),
                    alignment,
                );
            }
        }
    }
    Ok(PerseusAlignments { by_section })
}

fn needs_model(sec: &Section) -> bool {
    if sec.grc.is_empty() || sec.eng.is_empty() {
        return false;
    }
    // Fast path: count terminators directly without invoking the full
    // splitter. If neither side has more than one sentence's worth of
    // terminators we know the splitter will return ≤1 too — no model
    // call.
    let grc_terms = sec
        .grc
        .chars()
        .filter(|c| matches!(c, '.' | ';' | '·' | ':'))
        .count();
    let eng_terms = sec
        .eng
        .chars()
        .filter(|c| matches!(c, '.' | '?' | '!'))
        .count();
    // A trailing terminator counts as one "sentence end" without a
    // following sentence — only count >1 as multi-sentence.
    grc_terms > 1 || eng_terms > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_returns_single_group() {
        let a = SectionAlignment::trivial(
            vec![Sentence {
                text: "α.".into(),
                start: 0,
                end: 2,
            }],
            vec![Sentence {
                text: "A.".into(),
                start: 0,
                end: 2,
            }],
        );
        assert_eq!(a.groups.len(), 1);
        assert_eq!(a.groups[0].grc_indices, vec![0]);
        assert_eq!(a.groups[0].eng_indices, vec![0]);
    }

    #[test]
    fn trivial_with_zero_sentences_has_no_group() {
        let a = SectionAlignment::trivial(Vec::new(), Vec::new());
        assert!(a.groups.is_empty());
    }

    #[test]
    fn needs_model_skips_single_sentence_pairs() {
        let s = Section {
            n: "1".into(),
            grc: "Θουκυδίδης Ἀθηναῖος ξυνέγραψε.".into(),
            eng: "Thucydides wrote.".into(),
        };
        assert!(!needs_model(&s));
    }

    #[test]
    fn needs_model_picks_up_multi_sentence() {
        let s = Section {
            n: "1".into(),
            grc: "Πρώτη φράσις. Δεύτερη φράσις.".into(),
            eng: "First. Second.".into(),
        };
        assert!(needs_model(&s));
    }
}
