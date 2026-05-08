//! F4: Query parser. Tokenizes the search-bar string into structured filters
//! plus free-text. Pure function, table-driven tests.
//!
//! Syntax (Gmail-flavored, with Lucene-style escapes for the rough edges
//! Gmail leaves implicit):
//!
//! - `field:value` — include
//! - `-field:value` — exclude
//! - `field:"some value"` — quote when value has whitespace, `:`, leading
//!   `-`, or is empty
//! - Inside quotes: `\"` for literal quote, `\\` for literal backslash
//!
//! Each occurrence is its own AND clause downstream — repeating
//! `source:Slack channel:announce` zooms in tree-style; repeating the same
//! field with different values produces an empty result, which is the
//! correct read of "keep only X then keep only Y."

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Before,
    After,
    Subj,
    Type,
    Source,
    Kind,
    Channel,
    ConvoName,
    Author,
    Account,
    Project,
    Other(String),
}

impl Field {
    fn from_key(s: &str) -> Self {
        match s {
            "before" => Field::Before,
            "after" => Field::After,
            "subj" => Field::Subj,
            "type" => Field::Type,
            "source" => Field::Source,
            "kind" => Field::Kind,
            "channel" => Field::Channel,
            "convo" => Field::ConvoName,
            "author" => Field::Author,
            "account" => Field::Account,
            "project" => Field::Project,
            _ => Field::Other(s.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowType {
    Chat,
    Message,
    All,
}

/// One filter occurrence from the query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterTerm {
    pub field: Field,
    pub value: String,
    pub negate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Each occurrence preserved in order. Same-field repetitions are
    /// AND-ed downstream (tree-zoom).
    pub terms: Vec<FilterTerm>,
    /// Convenience grouping of positive terms by field — used by the few
    /// callers that want `IN (...)` semantics (none currently). Negative
    /// terms are not represented here.
    pub filters: BTreeMap<Field, Vec<String>>,
    pub free_text: String,
    pub resolved_type: RowType,
}

pub fn parse_query(s: &str) -> ParsedQuery {
    let mut terms: Vec<FilterTerm> = Vec::new();
    let mut filters: BTreeMap<Field, Vec<String>> = BTreeMap::new();
    let mut free_terms: Vec<String> = Vec::new();
    for tok in tokenize(s) {
        let (negate, body) = if let Some(rest) = tok.strip_prefix('-') {
            (true, rest.to_string())
        } else {
            (false, tok)
        };
        if let Some((k, v)) = split_field(&body) {
            if !k.is_empty() && !v.is_empty() {
                let field = Field::from_key(k);
                if !negate {
                    filters.entry(field.clone()).or_default().push(v.clone());
                }
                terms.push(FilterTerm {
                    field,
                    value: v,
                    negate,
                });
                continue;
            }
        }
        // Bare term: strip surrounding quotes so `"hello world"` lands as
        // `hello world` in free_text. Negation prefix without `field:`
        // falls back to free text including the leading `-` (we don't yet
        // do full-text NOT).
        let unquoted = unquote(&body);
        let raw = if negate {
            format!("-{}", unquoted)
        } else {
            unquoted
        };
        free_terms.push(raw);
    }
    let free_text = free_terms.join(" ");
    let resolved_type = match filters.get(&Field::Type).and_then(|v| v.first()) {
        Some(t) if t == "chat" => RowType::Chat,
        Some(t) if t == "message" => RowType::Message,
        Some(t) if t == "all" => RowType::All,
        _ if free_text.is_empty() => RowType::All,
        _ => RowType::Message,
    };
    ParsedQuery {
        terms,
        filters,
        free_text,
        resolved_type,
    }
}

/// Split a token at the first unquoted `:` into (key, unquoted_value).
/// Returns None if no `:` outside quotes is found.
fn split_field(tok: &str) -> Option<(&str, String)> {
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
                let key = &tok[..i];
                let val = unquote(&tok[i + 1..]);
                return Some((key, val));
            }
            _ => {}
        }
    }
    None
}

/// Strip surrounding quotes (if present) and unescape `\"` and `\\`.
fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escape = false;
        for ch in inner.chars() {
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
    } else {
        s.to_string()
    }
}

/// Whitespace-split, but respect double-quoted spans (with `\\` and `\"`
/// escapes) so that quoted values can contain spaces, colons, or quotes.
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

    fn one(field: Field, val: &str) -> BTreeMap<Field, Vec<String>> {
        let mut m = BTreeMap::new();
        m.insert(field, vec![val.to_string()]);
        m
    }

    #[test]
    fn empty_query_resolves_to_all() {
        let q = parse_query("");
        assert_eq!(q.resolved_type, RowType::All);
        assert_eq!(q.free_text, "");
        assert!(q.filters.is_empty());
        assert!(q.terms.is_empty());
    }

    #[test]
    fn free_text_resolves_to_message() {
        let q = parse_query("treemap layout");
        assert_eq!(q.resolved_type, RowType::Message);
        assert_eq!(q.free_text, "treemap layout");
    }

    #[test]
    fn type_override_wins() {
        let q = parse_query("treemap type:chat");
        assert_eq!(q.resolved_type, RowType::Chat);
        assert_eq!(q.free_text, "treemap");
    }

    #[test]
    fn structured_filters_collected() {
        let q = parse_query("before:2025-01-01 author:thad hello");
        assert_eq!(q.filters[&Field::Before], vec!["2025-01-01".to_string()]);
        assert_eq!(q.filters[&Field::Author], vec!["thad".to_string()]);
        assert_eq!(q.free_text, "hello");
    }

    #[test]
    fn unknown_field_preserved_as_other() {
        let q = parse_query("custom:foo");
        assert_eq!(q.filters, one(Field::Other("custom".into()), "foo"));
    }

    #[test]
    fn quoted_free_text() {
        let q = parse_query("\"hello world\" author:thad");
        assert_eq!(q.free_text, "hello world");
        assert_eq!(q.filters[&Field::Author], vec!["thad".to_string()]);
    }

    #[test]
    fn duplicate_filters_accumulate() {
        let q = parse_query("author:a author:b");
        assert_eq!(
            q.filters[&Field::Author],
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(q.terms.len(), 2);
        assert!(q.terms.iter().all(|t| !t.negate));
    }

    #[test]
    fn negation_recorded_in_terms_only() {
        let q = parse_query("-channel:announce");
        assert_eq!(q.terms.len(), 1);
        assert!(q.terms[0].negate);
        assert_eq!(q.terms[0].field, Field::Channel);
        assert_eq!(q.terms[0].value, "announce");
        // Negatives don't appear in `filters` (positive-only IN-style map).
        assert!(q.filters.is_empty());
    }

    #[test]
    fn quoted_value_with_special_chars() {
        let q = parse_query("channel:\"#dev:ops\"");
        let chan = &q.terms[0];
        assert_eq!(chan.field, Field::Channel);
        assert_eq!(chan.value, "#dev:ops");
        assert!(!chan.negate);
    }

    #[test]
    fn quoted_value_with_escapes() {
        let q = parse_query(r#"convo:"a\"b\\c""#);
        assert_eq!(q.terms[0].value, "a\"b\\c");
    }

    #[test]
    fn negated_quoted_value() {
        let q = parse_query(r#"-convo:"hello world""#);
        assert!(q.terms[0].negate);
        assert_eq!(q.terms[0].field, Field::ConvoName);
        assert_eq!(q.terms[0].value, "hello world");
    }

    #[test]
    fn source_and_kind_keys_recognized() {
        let q = parse_query("source:Slack kind:Chat");
        assert_eq!(q.filters[&Field::Source], vec!["Slack".to_string()]);
        assert_eq!(q.filters[&Field::Kind], vec!["Chat".to_string()]);
    }

    #[test]
    fn lone_dash_term_stays_free_text() {
        // `-foo` (no colon) is just a free-text token (we don't yet
        // implement free-text negation); the leading `-` is preserved so
        // round-tripping isn't lossy.
        let q = parse_query("-foo bar");
        assert_eq!(q.free_text, "-foo bar");
        assert!(q.terms.is_empty());
    }
}
