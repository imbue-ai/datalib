//! The search-bar grammar every grid here shares.
//!
//! A query is whitespace-separated tokens. `key:value` is a term, and a
//! leading `-` negates it. A double-quoted span may hold spaces, colons
//! and quotes (`\"` and `\\` escape inside it). Anything else is free
//! text, kept exactly as typed — quotes and leading `-` included — so a
//! consumer that forwards it to a search engine can keep its meaning.
//!
//! This is the grammar and nothing more. What a key names, and what free
//! text matches, belongs to whoever serves the rows: the unified grid maps
//! keys to `grid_rows` columns and sends free text to qmd; the run log
//! maps them to `log` columns and reads free text as a substring.

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
                Some((key, value)) if !key.is_empty() && !value.is_empty() => Token::Term(Term {
                    key: key.to_string(),
                    value,
                    negate,
                }),
                _ => Token::Free(tok),
            }
        })
        .collect()
}

/// The token for a term, quoted as the grammar needs. What "keep only" and
/// "exclude all" append to a query; `parse` reads it back as one `Term`.
pub fn term(key: &str, value: &str, negate: bool) -> String {
    format!("{}{key}:{}", if negate { "-" } else { "" }, quote(value))
}

/// `value` as one token: bare when it can be, double-quoted otherwise.
pub fn quote(value: &str) -> String {
    let bare = !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c == ':' || c == '"');
    if bare {
        return value.to_string();
    }
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

fn split_term(tok: &str) -> Option<(&str, String)> {
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
            ':' if !in_quote => return Some((&tok[..i], unquote(&tok[i + 1..]))),
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

fn tokenize(s: &str) -> Vec<String> {
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
        })
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
            vec![t("msg", "a: b", false), t("k", "say \"hi\" \\ done", false)]
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
            assert_eq!(parse(&tok), vec![t("k", value, true)], "{tok}");
        }
        assert_eq!(term("k", "plain", false), "k:plain");
        assert_eq!(term("k", "two words", false), "k:\"two words\"");
    }

    #[test]
    fn free_text_unquotes_and_reads_the_dash() {
        assert_eq!(free_text("word"), ("word".into(), false));
        assert_eq!(free_text("-\"earl grey\""), ("earl grey".into(), true));
        assert_eq!(free_text("-"), ("-".into(), false));
    }
}
