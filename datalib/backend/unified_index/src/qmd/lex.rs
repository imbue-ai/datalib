//! qmd's lex syntax in a free-text query: a quoted phrase, and a
//! `-`-prefixed exclusion. The daemon sends the text as is to qmd's
//! lexical search and without that syntax to its vector search.

pub fn has_lex_syntax(s: &str) -> bool {
    tokenize_query(s)
        .iter()
        .any(|t| t.starts_with('"') || t.starts_with('-'))
}

/// Strip qmd lex syntax from `s`: drop `-`-prefixed tokens entirely
/// (exclusions are meaningless to vector search), strip surrounding
/// quotes from phrases, and rejoin with single spaces. Returns an empty
/// string if every token is an exclusion.
pub fn strip_lex_syntax(s: &str) -> String {
    tokenize_query(s)
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

fn tokenize_query(s: &str) -> Vec<String> {
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
