//! The search-bar grammar every grid here shares.
//!
//! A query is whitespace-separated tokens. `key:value` is a term, and a
//! leading `-` negates it. A double-quoted span may hold spaces, colons
//! and quotes (`\"` and `\\` escape inside it). Anything else is free
//! text, kept exactly as typed — quotes and leading `-` included — so a
//! consumer that forwards it to a search engine can keep its meaning.
//!
//! What a key names, and what free text matches, belongs to whoever
//! serves the rows: the unified grid maps keys to `grid_rows` columns and
//! sends free text to qmd; the run log maps them to `log` columns and
//! reads free text as a substring. A table can say so in its schema:
//! `table` is the shape of that description.

pub mod table;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Term(Term),
    /// A token that is not `key:value`, verbatim.
    Free(String),
}

/// `key:value`, or `-key:value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub key: String,
    pub value: String,
    pub negate: bool,
    /// The value was written in double quotes: a key that matches in part
    /// matches it whole instead.
    pub quoted: bool,
}

pub fn parse(s: &str) -> Vec<Token> {
    tokenize(s)
        .into_iter()
        .map(|tok| {
            let (negate, body) = match tok.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, tok.as_str()),
            };
            match split_term(body) {
                Some((key, value, quoted)) if !key.is_empty() && !value.is_empty() => {
                    Token::Term(Term {
                        key: key.to_string(),
                        value,
                        negate,
                        quoted,
                    })
                }
                _ => Token::Free(tok),
            }
        })
        .collect()
}

/// The token for a term, quoted as the grammar needs. What "keep only" and
/// "exclude all" append to a query; `parse` reads it back as one `Term`.
pub fn term(key: &str, value: &str, negate: bool) -> String {
    let value = if bare(value, true) {
        value.to_string()
    } else {
        quoted(value)
    };
    format!("{}{key}:{value}", if negate { "-" } else { "" })
}

/// The token for a term whose value is matched whole: always quoted.
pub fn exact_term(key: &str, value: &str, negate: bool) -> String {
    format!("{}{key}:{}", if negate { "-" } else { "" }, quoted(value))
}

/// `value` as one token: bare when it can be, double-quoted otherwise.
pub fn quote(value: &str) -> String {
    if bare(value, false) {
        return value.to_string();
    }
    quoted(value)
}

/// Whether `value` reads back as itself unquoted. A term's value may hold
/// a colon, since a term splits at its first one (`from:email:a@b.c`); a
/// free word with one would read back as a term.
fn bare(value: &str, in_term: bool) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || (c == ':' && !in_term))
}

fn quoted(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// A free token with its quotes and escapes removed, and whether it was
/// negated. `-"earl grey"` is `("earl grey", true)`.
pub fn free_text(token: &str) -> (String, bool) {
    let (negate, body) = match token.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, token),
    };
    (unquote(body), negate)
}

fn split_term(tok: &str) -> Option<(&str, String, bool)> {
    let mut in_quote = false;
    let mut escape = false;
    for (i, ch) in tok.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_quote => escape = true,
            '"' => in_quote = !in_quote,
            ':' if !in_quote => {
                let raw = &tok[i + 1..];
                let quoted = raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"');
                return Some((&tok[..i], unquote(raw), quoted));
            }
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut escape = false;
    for ch in s[1..s.len() - 1].chars() {
        if escape {
            out.push(ch);
            escape = false;
        } else if ch == '\\' {
            escape = true;
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_quote => {
                cur.push('\\');
                escape = true;
            }
            '"' => {
                cur.push('"');
                in_quote = !in_quote;
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, value: &str, negate: bool) -> Token {
        Token::Term(Term {
            key: key.into(),
            value: value.into(),
            negate,
            quoted: false,
        })
    }

    fn tq(key: &str, value: &str, negate: bool) -> Token {
        Token::Term(Term {
            quoted: true,
            ..match t(key, value, negate) {
                Token::Term(term) => term,
                Token::Free(_) => unreachable!(),
            }
        })
    }

    /// A quoted value says so, whatever it holds: a key that matches in
    /// part reads it as the whole value.
    #[test]
    fn a_quoted_value_is_marked_and_exact_term_always_quotes() {
        assert_eq!(
            parse(r#"from:"Data" from:Data -from:"Lt. Cmdr. Data""#),
            vec![
                tq("from", "Data", false),
                t("from", "Data", false),
                tq("from", "Lt. Cmdr. Data", true),
            ]
        );
        assert_eq!(exact_term("from", "Data", false), r#"from:"Data""#);
        assert_eq!(
            parse(&exact_term("k", "a \"b\"", true)),
            vec![tq("k", "a \"b\"", true)]
        );
    }

    #[test]
    fn terms_and_free_text_in_order() {
        assert_eq!(
            parse("level:warn hello -target:sqlx \"two words\""),
            vec![
                t("level", "warn", false),
                Token::Free("hello".into()),
                t("target", "sqlx", true),
                Token::Free("\"two words\"".into()),
            ]
        );
    }

    #[test]
    fn quoted_values_hold_spaces_colons_and_escapes() {
        assert_eq!(
            parse(r#"msg:"a: b" k:"say \"hi\" \\ done""#),
            vec![
                tq("msg", "a: b", false),
                tq("k", "say \"hi\" \\ done", false)
            ]
        );
    }

    /// A lone `-`, an empty key or an empty value is not a term.
    #[test]
    fn malformed_terms_stay_free_text() {
        assert_eq!(
            parse("- :x key: -foo"),
            vec![
                Token::Free("-".into()),
                Token::Free(":x".into()),
                Token::Free("key:".into()),
                Token::Free("-foo".into()),
            ]
        );
    }

    /// Every value but the empty one: `k:""` reads back as free text, as
    /// an empty value always has, so no grid offers it as a filter.
    #[test]
    fn term_round_trips_through_parse() {
        for value in [
            "plain",
            "two words",
            "a:b",
            "-leading",
            "say \"hi\" \\ done",
        ] {
            let tok = term("k", value, true);
            let quoted = if tok.ends_with('"') { tq } else { t };
            assert_eq!(parse(&tok), vec![quoted("k", value, true)], "{tok}");
        }
        assert_eq!(term("k", "plain", false), "k:plain");
        assert_eq!(term("k", "two words", false), "k:\"two words\"");
        assert_eq!(term("k", "a:b", false), "k:a:b");
        assert_eq!(
            quote("a:b"),
            "\"a:b\"",
            "a free word with a colon would be a term"
        );
    }

    #[test]
    fn free_text_unquotes_and_reads_the_dash() {
        assert_eq!(free_text("word"), ("word".into(), false));
        assert_eq!(free_text("-\"earl grey\""), ("earl grey".into(), true));
        assert_eq!(free_text("-"), ("-".into(), false));
    }
}
