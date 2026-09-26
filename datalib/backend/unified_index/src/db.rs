//! Pure helpers used by `IndexRepo` implementations: dialect-agnostic
//! WHERE-builder and the [`ChatMeta`] row shape the impl returns. All SQL goes through `sqlx` against
//! [`crate::dolt_repo::DoltRepo`].

use crate::query::{extract_uuid_suffix, Field, ParsedQuery};
use datalib_schema::providers::Provider;

/// The source id datalib's own rows are filed under — the storage
/// reports, which describe a source's mirror rather than belonging to
/// it. Same spelling as their `provider` tag, because that tag is what
/// the filter and the Source column both test.
pub fn datalib_source_id() -> &'static str {
    Provider::Datalib.as_str()
}

/// Per-conversation header data read from `grid_rows`. The chat preview
/// renders the QMD body verbatim and pulls the page header from here —
/// no QMD parsing.
#[derive(Debug, Default, Clone)]
pub struct ChatMeta {
    /// The configured source's id (`grid_rows.source_id`); what the
    /// document view keys its per-source settings on.
    pub source_id: Option<String>,
    pub name: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
    pub created_at: Option<String>,
    pub source_label: Option<String>,
    /// Canonical web URL back to the provider, used for the page-level
    /// "Open in …" button.
    pub source_url: Option<String>,
}

/// Map a query [`Field`] to the underlying `grid_rows` column it
/// constrains, or `None` for fields that aren't single-column equality
/// filters (Before/After are range, Is sets `documents`, Subj/Other have
/// no column yet).
fn column_for_field(f: &Field) -> Option<&'static str> {
    match f {
        Field::Source => Some("source_label"),
        // Derived at index time: the path's first segment, or `datalib`
        // for a storage row (`GridRow::derived_source_id`).
        Field::SourceId => Some("source_id"),
        Field::Kind => Some("kind"),
        Field::Channel => Some("channel"),
        Field::Convo => Some("conversation_uuid"),
        Field::Author => Some("author"),
        Field::Account => Some("account"),
        Field::Project => Some("project"),
        Field::NotionPage => Some("notion_page_uuid"),
        Field::Change => Some("diff_status"),
        Field::Before | Field::After | Field::Is | Field::Subj | Field::Other(_) => None,
    }
}

/// Build the SQL `WHERE` clause (with a leading space) and the matching
/// parameter list for a parsed query's structured terms. Free text is not
/// here: it goes to qmd.
pub fn build_where(q: &ParsedQuery) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(documents) = q.documents {
        clauses.push(format!("is_document = {}", i32::from(documents)));
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
    // normalized to UTC first (datalib_time): a naive value means
    // local machine time, so it lands on the same basis as created_at_utc.
    // An unparseable bound drops the filter rather than compare garbage.
    if let Some(v) = q
        .filters
        .get(&Field::Before)
        .and_then(|vals| vals.first())
        .and_then(|v| datalib_time::normalize_user_time_to_utc(v))
    {
        clauses.push("created_at_utc < ?".into());
        params.push(v);
    }
    if let Some(v) = q
        .filters
        .get(&Field::After)
        .and_then(|vals| vals.first())
        .and_then(|v| datalib_time::normalize_user_time_to_utc(v))
    {
        clauses.push("created_at_utc > ?".into());
        params.push(v);
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
        let (sql, params) = build_where(&parse_query(""));
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn is_document_is_a_column_test_not_a_kind_list() {
        let (sql, params) = build_where(&parse_query("is:document"));
        assert_eq!(sql, " WHERE is_document = 1");
        assert!(params.is_empty());
        let (sql, _) = build_where(&parse_query("-is:document"));
        assert_eq!(sql, " WHERE is_document = 0");
    }

    #[test]
    fn source_filter_emits_equality_clause() {
        let (sql, params) = build_where(&parse_query("source:Claude"));
        assert_eq!(sql, " WHERE source_label = ?");
        assert_eq!(params, vec!["Claude"]);
    }

    /// `source:` and `source_id:` answer different questions: the
    /// provider label vs. the configured source. Two Slack workspaces are
    /// one `source` and two `source_id`s. What a row's `source_id` is,
    /// storage rows filed under datalib included, is decided once at index
    /// time (`GridRow::derived_source_id`); the filter only compares, so
    /// the `(source_id, …)` index can serve it.
    #[test]
    fn source_id_filter_is_equality_on_the_derived_column() {
        let (sql, params) = build_where(&parse_query("source_id:slack_work"));
        assert_eq!(sql, " WHERE source_id = ?");
        assert_eq!(params, vec!["slack_work"]);

        let (sql, params) = build_where(&parse_query("-source_id:datalib"));
        assert_eq!(sql, " WHERE (source_id IS NULL OR source_id != ?)");
        assert_eq!(params, vec!["datalib"]);
    }

    /// The old spelling has to reach the same SQL, not merely the same
    /// field: this is the assertion that a saved `source_name:` query
    /// keeps returning what it returned before the rename.
    #[test]
    fn the_old_source_name_spelling_builds_the_same_clause() {
        assert_eq!(
            build_where(&parse_query("source_name:slack")),
            build_where(&parse_query("source_id:slack")),
        );
    }

    #[test]
    fn negated_filter_keeps_nulls() {
        let (sql, _) = build_where(&parse_query("-channel:announce"));
        assert!(sql.contains("(channel IS NULL OR channel != ?)"));
    }

    /// `change:` is `diff_status`; negated it keeps NULL, so
    /// `-change:unchanged` is a diff's moved rows and every real row.
    #[test]
    fn change_filter_is_the_diff_status_column() {
        let (sql, params) = build_where(&parse_query("change:added"));
        assert!(sql.contains("diff_status = ?"), "{sql}");
        assert_eq!(params, vec!["added".to_string()]);
        let (sql, _) = build_where(&parse_query("-change:unchanged"));
        assert!(
            sql.contains("(diff_status IS NULL OR diff_status != ?)"),
            "{sql}"
        );
    }
}
