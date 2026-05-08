//! F4: Query parser. Tokenizes the search-bar string into structured filters
//! plus free-text. Pure function, table-driven tests.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Before,
    After,
    Subj,
    Type,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    pub filters: BTreeMap<Field, Vec<String>>,
    pub free_text: String,
    pub resolved_type: RowType,
}

pub fn parse_query(s: &str) -> ParsedQuery {
    let mut filters: BTreeMap<Field, Vec<String>> = BTreeMap::new();
    let mut free_terms: Vec<String> = Vec::new();
    for tok in tokenize(s) {
        if let Some((k, v)) = tok.split_once(':') {
            if !k.is_empty() && !v.is_empty() {
                filters
                    .entry(Field::from_key(k))
                    .or_default()
                    .push(v.to_string());
                continue;
            }
        }
        free_terms.push(tok);
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
        filters,
        free_text,
        resolved_type,
    }
}

/// Whitespace-split, but respect double-quoted spans for free-text and values.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
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
    }
}
