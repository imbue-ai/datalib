//! A words search from its terms' matches: each required word or phrase is
//! matched against messages on its own, and a document answers when every
//! one matched some message of it. Pure, so the reads stay plain
//! (`read.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};

/// One message a term matched, and its BM25 score there (higher is better).
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub markdown_uuid: String,
    pub seq: i64,
    pub start: i64,
    pub end: i64,
    pub score: f64,
}

/// A document a words search found, and the message it lands on.
#[derive(Debug, Clone, PartialEq)]
pub struct WordsHit {
    pub markdown_uuid: String,
    pub score: f64,
    pub start: i64,
    pub end: i64,
}

/// The documents every one of `required` matched and none of `excluded`
/// names, of `among` when given, best first, at most `limit`. A document
/// scores the sum over terms of the best score any of its messages got for
/// that term, and lands on the message holding the most terms, the higher
/// scoring of those that tie, the earlier of those.
pub fn rank(
    required: &[Vec<Matched>],
    excluded: &HashSet<String>,
    among: Option<&HashSet<String>>,
    limit: usize,
) -> Vec<WordsHit> {
    if required.is_empty() {
        return Vec::new();
    }
    struct Doc<'a> {
        best: Vec<f64>,
        // seq → (terms matched there, their summed score, the message)
        messages: BTreeMap<i64, (usize, f64, &'a Matched)>,
    }
    let mut docs: HashMap<&str, Doc> = HashMap::new();
    for (term, matches) in required.iter().enumerate() {
        let mut seen: HashSet<(&str, i64)> = HashSet::new();
        for m in matches {
            let doc = m.markdown_uuid.as_str();
            if excluded.contains(doc) || among.is_some_and(|a| !a.contains(doc)) {
                continue;
            }
            // A term reaches a document only when the one before it did.
            if term > 0 && !docs.contains_key(doc) {
                continue;
            }
            let entry = docs.entry(doc).or_insert_with(|| Doc {
                best: vec![f64::NEG_INFINITY; required.len()],
                messages: BTreeMap::new(),
            });
            entry.best[term] = entry.best[term].max(m.score);
            if seen.insert((doc, m.seq)) {
                let message = entry.messages.entry(m.seq).or_insert((0, 0.0, m));
                message.0 += 1;
                message.1 += m.score;
            }
        }
    }
    let mut hits: Vec<WordsHit> = docs
        .into_iter()
        .filter(|(_, d)| d.best.iter().all(|b| b.is_finite()))
        .map(|(uuid, d)| {
            let (_, _, landed) = d
                .messages
                .values()
                .fold(None::<&(usize, f64, &Matched)>, |best, m| match best {
                    Some(b) if (b.0, b.1) >= (m.0, m.1) => Some(b),
                    _ => Some(m),
                })
                .expect("a document reached here through a message");
            WordsHit {
                markdown_uuid: uuid.to_string(),
                score: d.best.iter().sum(),
                start: landed.start,
                end: landed.end,
            }
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

    fn m(doc: &str, seq: i64, score: f64) -> Matched {
        Matched {
            markdown_uuid: doc.into(),
            seq,
            start: seq * 100,
            end: seq * 100 + 100,
            score,
        }
    }

    fn docs(hits: &[WordsHit]) -> Vec<(&str, f64, i64)> {
        hits.iter()
            .map(|h| (h.markdown_uuid.as_str(), h.score, h.start / 100))
            .collect()
    }

    /// The plan's rule: a thread that says each word in a different message
    /// answers, as it did when qmd matched the whole thread.
    #[test]
    fn a_document_answers_when_each_term_matched_some_message_of_it() {
        let warp = vec![m("thread", 0, 2.0), m("memo", 0, 5.0)];
        let core = vec![m("thread", 3, 1.0)];
        let hits = rank(&[warp, core], &HashSet::new(), None, 10);
        assert_eq!(docs(&hits), [("thread", 3.0, 0)]);
    }

    #[test]
    fn a_hit_lands_on_the_message_holding_the_most_terms() {
        let warp = vec![m("thread", 0, 9.0), m("thread", 2, 1.0)];
        let core = vec![m("thread", 2, 1.0)];
        let hits = rank(&[warp, core], &HashSet::new(), None, 10);
        assert_eq!(docs(&hits), [("thread", 10.0, 2)]);
    }

    #[test]
    fn of_messages_holding_as_many_terms_the_better_scored_wins() {
        let warp = vec![
            m("thread", 0, 1.0),
            m("thread", 4, 3.0),
            m("thread", 7, 3.0),
        ];
        let hits = rank(&[warp], &HashSet::new(), None, 10);
        assert_eq!(docs(&hits), [("thread", 3.0, 4)]);
    }

    #[test]
    fn an_excluded_or_unasked_document_is_left_out_and_the_cut_is_by_score() {
        let warp = vec![
            m("a", 0, 1.0),
            m("b", 0, 3.0),
            m("c", 0, 2.0),
            m("d", 0, 4.0),
        ];
        let excluded = HashSet::from(["d".to_string()]);
        let among = HashSet::from(["a".to_string(), "b".to_string(), "c".to_string()]);
        let hits = rank(std::slice::from_ref(&warp), &excluded, Some(&among), 2);
        assert_eq!(docs(&hits), [("b", 3.0, 0), ("c", 2.0, 0)]);
        let all = rank(&[warp], &HashSet::new(), None, 10);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn nothing_required_finds_nothing() {
        assert!(rank(&[], &HashSet::new(), None, 10).is_empty());
    }
}
