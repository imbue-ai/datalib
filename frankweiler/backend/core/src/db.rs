//! Pure helpers used by `MirrorRepo` implementations: dialect-agnostic
//! WHERE-builder, snippet generator, and the [`ChatMeta`] row shape the
//! impl returns. All SQL goes through `sqlx` against
//! [`crate::dolt_repo::DoltRepo`].
//!
//! Both backends speak `?` placeholders and the same `grid_rows`
//! projection (column names + types written by `src/ingest/sql_writers.py`),
//! so a single WHERE-builder works for both.

use crate::query::{extract_uuid_suffix, Field, ParsedQuery, RowType};

const SNIPPET_LEN: usize = 240;

/// Per-conversation header data read from `grid_rows`. The chat preview
/// renders the QMD body verbatim and pulls the page header from here —
/// no QMD parsing.
#[derive(Debug, Default, Clone)]
pub struct ChatMeta {
    pub name: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
    pub when_ts: Option<String>,
    pub source_label: Option<String>,
    /// Canonical web URL back to the provider, used for the page-level
    /// "Open in …" button. For Slack rows `source_url` is null and we
    /// fall back to `slack_link` (a slack:// deep link) at SELECT time.
    pub source_url: Option<String>,
}

/// Build the snippet shown in the grid's "Contents" column. When the
/// query has a needle, center a 240-char window around the first match;
/// otherwise return the first 240 chars. Newlines become spaces so the
/// grid stays single-line.
pub fn snippet(text: &str, needle: &str) -> String {
    let trimmed = if needle.is_empty() {
        first_chars(text, SNIPPET_LEN)
    } else {
        let lower = text.to_lowercase();
        match lower.find(needle) {
            Some(pos) => {
                let radius = SNIPPET_LEN / 2;
                let start = text[..pos]
                    .char_indices()
                    .rev()
                    .nth(radius)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                let end_byte = pos + needle.len();
                let end = text[end_byte..]
                    .char_indices()
                    .nth(radius)
                    .map(|(i, _)| end_byte + i)
                    .unwrap_or(text.len());
                let mut out = String::new();
                if start > 0 {
                    out.push('…');
                }
                out.push_str(&text[start..end]);
                if end < text.len() {
                    out.push('…');
                }
                out
            }
            None => first_chars(text, SNIPPET_LEN),
        }
    };
    trimmed.replace('\n', " ")
}

fn first_chars(s: &str, n: usize) -> String {
    let end = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    let mut out = s[..end].to_string();
    if end < s.len() {
        out.push('…');
    }
    out
}

/// Map a query [`Field`] to the underlying `grid_rows` column it
/// constrains, or `None` for fields that aren't single-column equality
/// filters (Before/After are range, Type is a row-class classifier,
/// Subj/Other have no column yet).
fn column_for_field(f: &Field) -> Option<&'static str> {
    match f {
        Field::Source => Some("source_label"),
        Field::Kind => Some("kind"),
        Field::Channel => Some("channel"),
        Field::Convo => Some("conversation_uuid"),
        Field::Author => Some("author"),
        Field::Account => Some("account"),
        Field::Project => Some("project"),
        Field::NotionPage => Some("notion_page_uuid"),
        Field::Before | Field::After | Field::Type | Field::Subj | Field::Other(_) => None,
    }
}

/// Build the SQL `WHERE` clause (with a leading space) and the matching
/// parameter list for a parsed query. The output is portable between
/// MySQL (Dolt) and SQLite — `?` placeholders and `LOWER(text) LIKE ?`
/// both work on either dialect.
pub fn build_where(q: &ParsedQuery, needle: &str) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    match q.resolved_type {
        RowType::Chat => {
            clauses.push("kind IN ('Chat','Slack Thread')".into());
        }
        RowType::Message => {
            clauses.push("kind NOT IN ('Chat','Slack Thread')".into());
        }
        RowType::All => {}
    }

    // Per-term AND filters. Each occurrence is its own clause —
    // repeating the same field with different values produces an empty
    // result, which matches the "keep only X then keep only Y"
    // tree-zoom UX.
    for term in &q.terms {
        let Some(col) = column_for_field(&term.field) else {
            continue;
        };
        if term.negate {
            // Nullable columns: NULL would pass `col != ?` as unknown
            // and be dropped, which surprises users who didn't ask to
            // exclude unset values. Explicitly keep nulls.
            clauses.push(format!("({col} IS NULL OR {col} != ?)"));
        } else {
            clauses.push(format!("{col} = ?"));
        }
        let bound = if term.field.is_uuid_bearing() {
            extract_uuid_suffix(&term.value).to_string()
        } else {
            term.value.clone()
        };
        params.push(bound);
    }

    // Filter on the UTC-normalized index column, the same one the grid
    // sorts on, so before:/after: bounds agree with display order across
    // rows recorded in different local offsets. The user-typed bound is
    // normalized to UTC first (frankweiler_time): a naive value means
    // local machine time, so it lands on the same basis as when_ts_utc.
    // An unparseable bound drops the filter rather than compare garbage.
    if let Some(v) = q
        .filters
        .get(&Field::Before)
        .and_then(|vals| vals.first())
        .and_then(|v| frankweiler_time::normalize_user_time_to_utc(v))
    {
        clauses.push("when_ts_utc < ?".into());
        params.push(v);
    }
    if let Some(v) = q
        .filters
        .get(&Field::After)
        .and_then(|vals| vals.first())
        .and_then(|v| frankweiler_time::normalize_user_time_to_utc(v))
    {
        clauses.push("when_ts_utc > ?".into());
        params.push(v);
    }
    if !needle.is_empty() {
        clauses.push("LOWER(text) LIKE ?".into());
        params.push(format!("%{}%", needle));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;

    #[test]
    fn empty_query_produces_no_where() {
        let (sql, params) = build_where(&parse_query("type:all"), "");
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn source_filter_emits_equality_clause() {
        let (sql, params) = build_where(&parse_query("source:Claude type:all"), "");
        assert_eq!(sql, " WHERE source_label = ?");
        assert_eq!(params, vec!["Claude"]);
    }

    #[test]
    fn negated_filter_keeps_nulls() {
        let (sql, _) = build_where(&parse_query("-channel:announce type:all"), "");
        assert!(sql.contains("(channel IS NULL OR channel != ?)"));
    }

    #[test]
    fn free_text_becomes_lower_like() {
        let (sql, params) = build_where(&parse_query("hello"), "hello");
        // `hello` is not a field:value, so it resolves to message type.
        assert!(sql.contains("LOWER(text) LIKE ?"));
        assert!(params.iter().any(|p| p == "%hello%"));
    }

    #[test]
    fn snippet_centers_window_around_needle() {
        let text = "a".repeat(200) + "needle" + &"b".repeat(200);
        let out = snippet(&text, "needle");
        assert!(out.contains("needle"));
        assert!(out.starts_with('…'));
        assert!(out.ends_with('…'));
    }
}
