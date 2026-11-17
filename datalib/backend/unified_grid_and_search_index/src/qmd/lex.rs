//! The syntax free text may carry: a quoted phrase, and a `-`-prefixed
//! exclusion. A search by words asks it of the keyword index as FTS5; a
//! search by meaning embeds the text without it.

use datalib_query::tokenize;

pub fn has_lex_syntax(s: &str) -> bool {
    tokenize(s)
        .iter()
        .any(|t| t.starts_with('"') || t.starts_with('-'))
}

/// Strip that syntax from `s`: drop `-`-prefixed tokens entirely
/// (exclusions are meaningless to vector search), strip surrounding
/// quotes from phrases, and rejoin with single spaces. Returns an empty
/// string if every token is an exclusion.
pub fn strip_lex_syntax(s: &str) -> String {
    tokenize(s)
        .into_iter()
        .filter(|t| !t.starts_with('-'))
        .map(|t| strip_outer_quotes(&t).to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_outer_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Free text as an FTS5 query over the keyword index's titles and bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fts5Query {
    pub expr: String,
}

/// Every word and quoted phrase required, a `-` one excluded. `None` when
/// nothing is required, which FTS5 cannot ask.
pub fn fts5_query(s: &str) -> Option<Fts5Query> {
    let quote = |t: &str| format!("\"{}\"", strip_outer_quotes(t).replace('"', "\"\""));
    let mut required: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for token in tokenize(s) {
        match token.strip_prefix('-').filter(|t| !t.is_empty()) {
            Some(t) => excluded.push(quote(t)),
            None => required.push(quote(&token)),
        }
    }
    if required.is_empty() {
        return None;
    }
    let mut expr = required.join(" AND ");
    for e in excluded {
        expr.push_str(" NOT ");
        expr.push_str(&e);
    }
    Some(Fts5Query {
        expr: format!("{{title body}} : ({expr})"),
    })
}

/// Several plain words as one phrase: the documents holding them in the
/// order typed, which the Words tab ranks above those holding them
/// anywhere. `None` for one word, or for text with its own phrases or
/// exclusions, which already says what it wants.
pub fn fts5_phrase_query(s: &str) -> Option<Fts5Query> {
    let tokens = tokenize(s);
    if tokens.len() < 2 || tokens.iter().any(|t| t.contains('"') || t.starts_with('-')) {
        return None;
    }
    let phrase = tokens.join(" ");
    Some(Fts5Query {
        expr: format!("{{title body}} : (\"{phrase}\")"),
    })
}

/// The words a page may hold across its pieces, each its own FTS5 query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTerms {
    pub required: Vec<String>,
    pub excluded: Vec<String>,
}

/// Each word and quoted phrase required, and each `-` one excluded, apart:
/// a page holds the words between its pieces where a message holds them
/// all at once. `None` for fewer than two required, which [`fts5_query`]
/// already asks of each piece.
pub fn fts5_page_terms(s: &str) -> Option<PageTerms> {
    let one = |t: &str| {
        format!(
            "{{title body}} : (\"{}\")",
            strip_outer_quotes(t).replace('"', "\"\"")
        )
    };
    let mut terms = PageTerms {
        required: Vec::new(),
        excluded: Vec::new(),
    };
    for token in tokenize(s) {
        match token.strip_prefix('-').filter(|t| !t.is_empty()) {
            Some(t) => terms.excluded.push(one(t)),
            None => terms.required.push(one(&token)),
        }
    }
    (terms.required.len() >= 2).then_some(terms)
}

#[cfg(test)]
mod fts5_tests {
    use super::*;

    #[test]
    fn plain_words_make_one_phrase_and_typed_syntax_makes_none() {
        assert_eq!(
            fts5_phrase_query("warp core breach"),
            Some(Fts5Query {
                expr: r#"{title body} : ("warp core breach")"#.into(),
            })
        );
        assert_eq!(fts5_phrase_query("warp"), None);
        assert_eq!(fts5_phrase_query("warp \"core breach\""), None);
        assert_eq!(fts5_phrase_query("warp -core"), None);
    }

    #[test]
    fn words_are_required_phrases_kept_and_exclusions_negated() {
        assert_eq!(
            fts5_query("warp core \"deck twelve\" -breach"),
            Some(Fts5Query {
                expr: r#"{title body} : ("warp" AND "core" AND "deck twelve" NOT "breach")"#.into(),
            })
        );
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
    fn a_pages_terms_are_asked_one_at_a_time() {
        assert_eq!(
            fts5_page_terms("bedroom \"air quality\" -kitchen"),
            Some(PageTerms {
                required: vec![
                    r#"{title body} : ("bedroom")"#.into(),
                    r#"{title body} : ("air quality")"#.into(),
                ],
                excluded: vec![r#"{title body} : ("kitchen")"#.into()],
            })
        );
        assert_eq!(fts5_page_terms("bedroom -kitchen"), None);
    }

    #[test]
    fn only_exclusions_ask_nothing() {
        assert_eq!(fts5_query("-spam"), None);
        assert_eq!(fts5_query(""), None);
    }
}
