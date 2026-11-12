//! A words search from its terms' matches: each required word or phrase is
//! matched against messages on its own, and a document answers when every
//! one matched some message of it. Pure, so the reads stay plain
//! (`read.rs`).

use std::collections::{HashMap, HashSet};

/// A document a term matched, and the best BM25 score any one of its
/// messages got for it (higher is better).
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub markdown_uuid: String,
    pub score: f64,
}

/// A document a words search found.
#[derive(Debug, Clone, PartialEq)]
pub struct WordsHit {
    pub markdown_uuid: String,
    pub score: f64,
}

/// The documents every one of `required` matched and none of `excluded`
/// names, of `among` when given, best first, at most `limit`. A document
/// scores the sum of its scores for each term.
pub fn rank(
    required: &[Vec<Matched>],
    excluded: &HashSet<String>,
    among: Option<&HashSet<String>>,
    limit: usize,
) -> Vec<WordsHit> {
    let Some((first, rest)) = required.split_first() else {
        return Vec::new();
    };
    let mut docs: HashMap<&str, (usize, f64)> = HashMap::new();
    for m in first {
        let doc = m.markdown_uuid.as_str();
        if excluded.contains(doc) || among.is_some_and(|a| !a.contains(doc)) {
            continue;
        }
        let entry = docs.entry(doc).or_insert((1, f64::NEG_INFINITY));
        entry.1 = entry.1.max(m.score);
    }
    for (term, matches) in rest.iter().enumerate() {
        let mut best: HashMap<&str, f64> = HashMap::new();
        for m in matches {
            let b = best
                .entry(m.markdown_uuid.as_str())
                .or_insert(f64::NEG_INFINITY);
            *b = b.max(m.score);
        }
        for (doc, score) in best {
            if let Some(entry) = docs.get_mut(doc).filter(|e| e.0 == term + 1) {
                entry.0 += 1;
                entry.1 += score;
            }
        }
    }
    let mut hits: Vec<WordsHit> = docs
        .into_iter()
        .filter(|(_, (terms, _))| *terms == required.len())
        .map(|(uuid, (_, score))| WordsHit {
            markdown_uuid: uuid.to_string(),
            score,
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.markdown_uuid.cmp(&b.markdown_uuid))
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(doc: &str, score: f64) -> Matched {
        Matched {
            markdown_uuid: doc.into(),
            score,
        }
    }

    fn docs(hits: &[WordsHit]) -> Vec<(&str, f64)> {
        hits.iter()
            .map(|h| (h.markdown_uuid.as_str(), h.score))
            .collect()
    }

    /// The plan's rule: a document answers when every term matched it, in
    /// whichever of its messages; it scores the sum of its best per term.
    #[test]
    fn a_document_answers_when_each_term_matched_it() {
        let warp = vec![m("thread", 2.0), m("memo", 5.0)];
        let core = vec![m("thread", 1.0)];
        let hits = rank(&[warp, core], &HashSet::new(), None, 10);
        assert_eq!(docs(&hits), [("thread", 3.0)]);
    }

    #[test]
    fn a_term_counts_once_a_document_at_its_best() {
        let warp = vec![m("thread", 1.0), m("thread", 3.0)];
        let core = vec![m("thread", 1.0), m("thread", 0.5)];
        let hits = rank(&[warp, core], &HashSet::new(), None, 10);
        assert_eq!(docs(&hits), [("thread", 4.0)]);
    }

    #[test]
    fn an_excluded_or_unasked_document_is_left_out_and_the_cut_is_by_score() {
        let warp = vec![m("a", 1.0), m("b", 3.0), m("c", 2.0), m("d", 4.0)];
        let excluded = HashSet::from(["d".to_string()]);
        let among = HashSet::from(["a".to_string(), "b".to_string(), "c".to_string()]);
        let hits = rank(std::slice::from_ref(&warp), &excluded, Some(&among), 2);
        assert_eq!(docs(&hits), [("b", 3.0), ("c", 2.0)]);
        let all = rank(&[warp], &HashSet::new(), None, 10);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn nothing_required_finds_nothing() {
        assert!(rank(&[], &HashSet::new(), None, 10).is_empty());
    }
}
