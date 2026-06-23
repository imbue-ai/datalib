//! Within-section sentence-level alignment between a *pair* of editions.
//!
//! The Perseus CTS spine already gives us section-level alignment for
//! free (`1.4.1` exists in every edition). What this module adds is the
//! next layer: when one edition split a sentence the other kept whole
//! (or vice versa), find which sentence goes with which.
//!
//! Alignment is **opt-in per edition pair** — the source config's
//! `alignment_pairs` lists the `(edition_a, edition_b)` pairs to align
//! (default: none). For each configured pair and each section both
//! cover, we:
//!   1. Split each side into sentences with [`split::split_for`] (the
//!      splitter is chosen by the edition's language).
//!   2. If both sides have ≤1 sentence, return the trivial 1:1 (no
//!      model call).
//!   3. Otherwise embed each sentence via [`embed::Embedder`]
//!      (mean-pooled Ancient-Greek-BERT) and run [`dp::align`].
//!
//! The result is per (book, chapter, section) a list of
//! [`SectionPairAlignment`]s — one per configured pair that covers the
//! section — each carrying which a-sentence-indices group with which
//! b-sentence-indices. The renderer turns those into `<span>` anchors
//! and `bilingual-alignment` edges.

pub mod dp;
pub mod embed;
pub mod split;

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::render_and_index_md::parse::ParsedPerseus;
pub use embed::Embedder;
pub use split::Sentence;

/// One aligned grouping inside a section: indices into edition a's
/// sentence split ↔ indices into edition b's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairGroup {
    pub a: Vec<usize>,
    pub b: Vec<usize>,
}

/// Alignment of one section across one configured edition pair.
#[derive(Debug, Clone)]
pub struct SectionPairAlignment {
    pub a_id: String,
    pub b_id: String,
    pub groups: Vec<PairGroup>,
}

/// All per-section alignments for a work, plus the set of editions that
/// participate in any pair (so the renderer knows which editions to
/// wrap in per-sentence anchor spans).
#[derive(Debug, Default)]
pub struct PerseusAlignments {
    by_section: HashMap<(String, String, String), Vec<SectionPairAlignment>>,
    aligned: HashSet<String>,
}

impl PerseusAlignments {
    /// The pair alignments covering one section (empty when none).
    pub fn for_section(&self, book_n: &str, ch_n: &str, sec_n: &str) -> &[SectionPairAlignment] {
        self.by_section
            .get(&(book_n.to_string(), ch_n.to_string(), sec_n.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether an edition participates in any configured pair — i.e.
    /// whether the renderer should emit per-sentence anchor spans for
    /// it.
    pub fn is_aligned(&self, edition_id: &str) -> bool {
        self.aligned.contains(edition_id)
    }
}

/// Align every configured pair across the work. Loads the embedder once
/// iff at least one (pair, section) is non-trivial. With `pairs` empty
/// this is a cheap no-op returning empty alignments.
pub async fn align_all(
    parsed: &ParsedPerseus,
    pairs: &[(String, String)],
) -> Result<PerseusAlignments> {
    // Keep only pairs whose editions both exist in this corpus.
    let valid: Vec<(String, String)> = pairs
        .iter()
        .filter(|(a, b)| {
            parsed.editions.iter().any(|e| &e.id == a) && parsed.editions.iter().any(|e| &e.id == b)
        })
        .cloned()
        .collect();
    if valid.is_empty() {
        return Ok(PerseusAlignments::default());
    }

    let mut aligned: HashSet<String> = HashSet::new();
    for (a, b) in &valid {
        aligned.insert(a.clone());
        aligned.insert(b.clone());
    }

    // Does any (pair, section) need the model? If not, skip the load.
    let needs_model = valid.iter().any(|(a, b)| {
        let (la, lb) = (parsed.lang_of(a), parsed.lang_of(b));
        parsed.books.iter().any(|book| {
            book.chapters.iter().any(|ch| {
                ch.sections.iter().any(|s| {
                    let (at, bt) = (s.text(a), s.text(b));
                    !at.is_empty()
                        && !bt.is_empty()
                        && (split::split_for(la, at).len() > 1
                            || split::split_for(lb, bt).len() > 1)
                })
            })
        })
    });
    tracing::info!(
        "perseus alignment: {} pair(s), model {}",
        valid.len(),
        if needs_model { "loading" } else { "not needed" }
    );
    let emb = if needs_model {
        Some(Embedder::load().await?)
    } else {
        None
    };

    let mut by_section: HashMap<(String, String, String), Vec<SectionPairAlignment>> =
        HashMap::new();
    for (a, b) in &valid {
        let (la, lb) = (parsed.lang_of(a).to_string(), parsed.lang_of(b).to_string());
        for book in &parsed.books {
            for ch in &book.chapters {
                for sec in &ch.sections {
                    let (at, bt) = (sec.text(a), sec.text(b));
                    if at.is_empty() || bt.is_empty() {
                        continue;
                    }
                    let groups = align_pair(emb.as_ref(), at, &la, bt, &lb)?;
                    by_section
                        .entry((book.n.clone(), ch.n.clone(), sec.n.clone()))
                        .or_default()
                        .push(SectionPairAlignment {
                            a_id: a.clone(),
                            b_id: b.clone(),
                            groups,
                        });
                }
            }
        }
    }
    Ok(PerseusAlignments {
        by_section,
        aligned,
    })
}

/// Align one section's two texts. Returns one trivial group when
/// neither side has more than one sentence (no model call).
fn align_pair(
    emb: Option<&Embedder>,
    a_text: &str,
    a_lang: &str,
    b_text: &str,
    b_lang: &str,
) -> Result<Vec<PairGroup>> {
    let a = split::split_for(a_lang, a_text);
    let b = split::split_for(b_lang, b_text);

    if (a.len() <= 1 && b.len() <= 1) || a.is_empty() || b.is_empty() {
        let ga: Vec<usize> = (0..a.len()).collect();
        let gb: Vec<usize> = (0..b.len()).collect();
        return Ok(if ga.is_empty() && gb.is_empty() {
            Vec::new()
        } else {
            vec![PairGroup { a: ga, b: gb }]
        });
    }

    let emb = emb.expect("embedder loaded when a section is non-trivial");
    let a_emb: Vec<Vec<f32>> = a
        .iter()
        .map(|s| emb.embed_one(&s.text))
        .collect::<Result<_>>()?;
    let b_emb: Vec<Vec<f32>> = b
        .iter()
        .map(|s| emb.embed_one(&s.text))
        .collect::<Result<_>>()?;
    let a_lens: Vec<usize> = a.iter().map(|s| s.text.chars().count()).collect();
    let b_lens: Vec<usize> = b.iter().map(|s| s.text.chars().count()).collect();
    let groups = dp::align(&a_emb, &b_emb, &a_lens, &b_lens)
        .into_iter()
        .map(|g| PairGroup { a: g.grc, b: g.eng })
        .collect();
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_pairs_is_a_no_op() {
        let parsed = ParsedPerseus::default();
        let al = align_all(&parsed, &[]).await.unwrap();
        assert!(!al.is_aligned("perseus-grc2"));
        assert!(al.for_section("1", "1", "1").is_empty());
    }

    #[test]
    fn trivial_pair_needs_no_model() {
        // One sentence each side → trivial group, emb unused.
        let groups = align_pair(None, "Μία πρότασις.", "grc", "One sentence.", "eng").unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].a, vec![0]);
        assert_eq!(groups[0].b, vec![0]);
    }
}
