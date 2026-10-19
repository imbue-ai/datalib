//! qmd's lex syntax in a free-text query: a quoted phrase, and a
//! `-`-prefixed exclusion. The daemon sends the text as is to qmd's
//! lexical search and without that syntax to its vector search.

use datalib_query::tokenize;

pub fn has_lex_syntax(s: &str) -> bool {
    tokenize(s)
        .iter()
        .any(|t| t.starts_with('"') || t.starts_with('-'))
}

/// Strip qmd lex syntax from `s`: drop `-`-prefixed tokens entirely
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

/// Free text as an FTS5 query over qmd's `documents_fts` titles and bodies,
/// for the Words tab, which reads that table itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fts5Query {
    pub expr: String,
    /// The first word or phrase it requires, as typed: what the line a hit
    /// lands on is looked for by.
    pub first: String,
}

/// Every word and quoted phrase required, a `-` one excluded. `None` when
/// nothing is required, which FTS5 cannot ask.
pub fn fts5_query(s: &str) -> Option<Fts5Query> {
    let quote = |t: &str| format!("\"{}\"", strip_outer_quotes(t).replace('"', "\"\""));
    let mut required: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut first: Option<String> = None;
    for token in tokenize(s) {
        match token.strip_prefix('-').filter(|t| !t.is_empty()) {
            Some(t) => excluded.push(quote(t)),
            None => {
                first.get_or_insert_with(|| strip_outer_quotes(&token).to_string());
                required.push(quote(&token));
            }
        }
    }
    let first = first?;
    let mut expr = required.join(" AND ");
    for e in excluded {
        expr.push_str(" NOT ");
        expr.push_str(&e);
    }
    Some(Fts5Query {
        expr: format!("{{title body}} : ({expr})"),
        first,
    })
}

#[cfg(test)]
mod fts5_tests {
    use super::*;

    #[test]
    fn words_are_required_phrases_kept_and_exclusions_negated() {
        assert_eq!(
            fts5_query("warp core \"deck twelve\" -breach"),
            Some(Fts5Query {
                expr: r#"{title body} : ("warp" AND "core" AND "deck twelve" NOT "breach")"#.into(),
                first: "warp".into(),
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
    fn only_exclusions_ask_nothing() {
        assert_eq!(fts5_query("-spam"), None);
        assert_eq!(fts5_query(""), None);
    }
}
