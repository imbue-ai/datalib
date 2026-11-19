//! A search's words to rank by (`ParsedQuery::free_text`) as FTS5 queries
//! over the keyword index's titles and bodies. Quotes, `+` and `-` are
//! filters on a row's text by the time the words get here (`db.rs`).

use datalib_query::tokenize;

use crate::db::fts5_phrase;

/// Free text as an FTS5 query over the keyword index's titles and bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fts5Query {
    pub expr: String,
}

/// Every word required. `None` with no word.
pub fn fts5_query(s: &str) -> Option<Fts5Query> {
    let words: Vec<String> = tokenize(s).iter().map(|w| quoted(w)).collect();
    (!words.is_empty()).then(|| Fts5Query {
        expr: format!("{{title body}} : ({})", words.join(" AND ")),
    })
}

/// Several words as one phrase: the documents holding them in the order
/// typed, which a search ranks above those holding them anywhere. `None`
/// for one word.
pub fn fts5_phrase_query(s: &str) -> Option<Fts5Query> {
    let words = tokenize(s);
    (words.len() >= 2).then(|| Fts5Query {
        expr: fts5_phrase(&words.join(" ")),
    })
}

/// Each word apart, each its own FTS5 query: a page holds the words
/// between its pieces where a message holds them all at once. `None` for
/// fewer than two, which [`fts5_query`] already asks of each piece.
pub fn fts5_page_terms(s: &str) -> Option<Vec<String>> {
    let words: Vec<String> = tokenize(s).iter().map(|w| fts5_phrase(w)).collect();
    (words.len() >= 2).then_some(words)
}

fn quoted(word: &str) -> String {
    format!("\"{}\"", word.replace('"', "\"\""))
}

#[cfg(test)]
mod fts5_tests {
    use super::*;

    #[test]
    fn several_words_make_one_phrase() {
        assert_eq!(
            fts5_phrase_query("warp core breach"),
            Some(Fts5Query {
                expr: r#"{title body} : ("warp core breach")"#.into(),
            })
        );
        assert_eq!(fts5_phrase_query("warp"), None);
    }

    #[test]
    fn every_word_is_required() {
        assert_eq!(
            fts5_query("warp core deck"),
            Some(Fts5Query {
                expr: r#"{title body} : ("warp" AND "core" AND "deck")"#.into(),
            })
        );
        assert_eq!(fts5_query(""), None);
    }

    /// A quote typed inside a word cannot end the phrase it is put in.
    #[test]
    fn a_quote_inside_a_word_is_doubled() {
        assert_eq!(
            fts5_query("o\"brien").map(|q| q.expr),
            Some(r#"{title body} : ("o""brien")"#.into())
        );
    }

    #[test]
    fn a_pages_words_are_asked_one_at_a_time() {
        assert_eq!(
            fts5_page_terms("bedroom air"),
            Some(vec![
                r#"{title body} : ("bedroom")"#.to_string(),
                r#"{title body} : ("air")"#.to_string(),
            ])
        );
        assert_eq!(fts5_page_terms("bedroom"), None);
    }
}
