//! Pure helpers used by `IndexRepo` implementations: dialect-agnostic
//! WHERE-builder and the [`ChatMeta`] row shape the impl returns. All SQL goes through `sqlx` against
//! [`crate::dolt_repo::DoltRepo`].

use crate::group::like_pattern;
use crate::query::{extract_uuid_suffix, Field, FilterTerm, ParsedQuery};
use crate::terms_keys::{self, TermsKey, TermsValue, ATTACHED_AS};
use datalib_query::table::{Column, FreeText, SearchTable};
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

/// A term's value that stands for any value at all: `author:*`.
pub const ANY_VALUE: &str = "*";

/// Build the SQL `WHERE` clause (with a leading space) and the matching
/// parameter list for a parsed query's terms, and for its free text when
/// the table matches free text with `LIKE`; a qmd table's free text is
/// qmd's.
pub fn build_where<C: Column>(q: &ParsedQuery<C>) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for (_, column) in <C::Table as SearchTable>::FLAGS {
        if let Some(on) = q.flag(*column) {
            clauses.push(format!("{} = {}", column.as_str(), i32::from(on)));
        }
    }

    // Per-term AND filters. Each occurrence is its own clause —
    // repeating the same field with different values produces an empty
    // result, which matches the "keep only X then keep only Y"
    // tree-zoom UX.
    for term in &q.terms {
        if let Field::Terms(key) = term.field {
            let pk = <C::Table as SearchTable>::PRIMARY_KEY.as_str();
            let (clause, bound) = terms_clause(pk, key, term);
            clauses.push(clause);
            params.extend(bound);
            continue;
        }
        // Before/after are a range, below; `is:` is a flag, above.
        let Field::Column(key) = term.field else {
            continue;
        };
        let col = key.column.as_str();
        // `author:*` is the rows with an author, `-author:*` the rows with
        // none; an empty value is none, as the grid shows it.
        if term.value == ANY_VALUE {
            clauses.push(if term.negate {
                format!("({col} IS NULL OR {col} = '')")
            } else {
                format!("({col} IS NOT NULL AND {col} != '')")
            });
            continue;
        }
        if term.negate {
            // Nullable columns: NULL would pass `col != ?` as unknown
            // and be dropped, which surprises users who didn't ask to
            // exclude unset values. Explicitly keep nulls.
            clauses.push(format!("({col} IS NULL OR {col} != ?)"));
        } else {
            clauses.push(format!("{col} = ?"));
        }
        let bound = if key.uuid {
            extract_uuid_suffix(&term.value).to_string()
        } else {
            term.value.clone()
        };
        params.push(bound);
    }

    // The range column is a UTC stamp, the one the grid sorts on, so
    // before:/after: bounds agree with display order across rows recorded
    // in different local offsets. The user-typed bound is normalized to
    // UTC first (datalib_time): a naive value means local machine time.
    // An unparseable bound drops the filter rather than compare garbage.
    if let Some(range) = <C::Table as SearchTable>::RANGE {
        for (field, op) in [(Field::Before, "<"), (Field::After, ">")] {
            if let Some(v) = q
                .bound(field)
                .and_then(datalib_time::normalize_user_time_to_utc)
            {
                clauses.push(format!("{} {op} ?", range.as_str()));
                params.push(v);
            }
        }
    }

    if let (FreeText::Like(columns), false) =
        (<C::Table as SearchTable>::FREE_TEXT, q.free_text.is_empty())
    {
        let needle = format!("%{}%", q.free_text.to_lowercase());
        let any: Vec<String> = columns
            .iter()
            .map(|c| format!("LOWER(COALESCE({}, '')) LIKE ?", c.as_str()))
            .collect();
        clauses.push(format!("({})", any.join(" OR ")));
        params.extend(columns.iter().map(|_| needle.clone()));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

/// The rows whose search terms of `key`'s kinds hold `value`, as one
/// clause over `pk` and the values it binds. The kinds are the enum's own
/// codes, so they are written in; the value is bound.
fn terms_clause<C>(pk: &str, key: &TermsKey, term: &FilterTerm<C>) -> (String, Vec<String>) {
    let s = ATTACHED_AS;
    let codes: Vec<String> = key.kinds().iter().map(|k| k.code().to_string()).collect();
    let (matched, bound) = match terms_keys::value_of(key, &term.value, term.quoted, ANY_VALUE) {
        TermsValue::Any => (String::new(), Vec::new()),
        // `vals_nocase` and `names_by_name` serve each `=`: a whole value,
        // case-blind.
        TermsValue::Exact { values, by_name } => {
            let mut any: Vec<String> = values
                .iter()
                .map(|_| "value = ? COLLATE NOCASE".to_string())
                .collect();
            let mut bound = values;
            if let Some(name) = by_name {
                any.push(format!(
                    "value IN (SELECT handle FROM {s}.names WHERE name = ? COLLATE NOCASE)"
                ));
                bound.push(name);
            }
            (
                format!(
                    " AND t.val_id IN (SELECT val_id FROM {s}.vals WHERE {})",
                    any.join(" OR ")
                ),
                bound,
            )
        }
        TermsValue::Partial { text, by_name } => {
            let pattern = like_pattern(&text);
            let names = if by_name {
                format!(
                    " OR value IN (SELECT handle FROM {s}.names \
                     WHERE LOWER(name) LIKE ? ESCAPE '\\')"
                )
            } else {
                String::new()
            };
            let bound = if by_name {
                vec![pattern.clone(), pattern]
            } else {
                vec![pattern]
            };
            (
                format!(
                    " AND t.val_id IN (SELECT val_id FROM {s}.vals \
                     WHERE LOWER(value) LIKE ? ESCAPE '\\'{names})"
                ),
                bound,
            )
        }
        // A contact is its handles, read from the contacts store before the
        // query runs; none read is no row (`IN ()`), and the applet refuses
        // a contact it could not read before it gets here.
        TermsValue::Contact(_) => {
            let handles = term.handles.clone().unwrap_or_default();
            (
                format!(
                    " AND t.val_id IN (SELECT val_id FROM {s}.vals WHERE value IN ({}))",
                    vec!["?"; handles.len()].join(", ")
                ),
                handles,
            )
        }
    };
    let not = if term.negate { "NOT " } else { "" };
    (
        format!(
            "{pk} {not}IN (SELECT r.uuid FROM {s}.terms t JOIN {s}.rows r ON r.row_id = t.row_id \
             WHERE t.kind IN ({}){matched})",
            codes.join(", ")
        ),
        bound,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;

    /// A table the tests declare, shaped like the problems table: free
    /// text is a substring of its columns, and it has no range and no
    /// flags.
    mod log {
        #[allow(dead_code)]
        #[derive(datalib_etl_macros::PortableTable)]
        #[portable_table(table = "log", primary_key = "seq", search(order = "seq desc"))]
        pub struct Entry {
            #[col(sql = "INTEGER")]
            pub seq: i64,
            #[col(sql = "VARCHAR(32)", search = "who", alias = "officer", like)]
            pub author: Option<String>,
            #[col(sql = "TEXT", like)]
            pub body: String,
        }
    }
    use log::EntryColumn;

    #[test]
    fn a_like_table_matches_free_text_in_its_columns() {
        let q = ParsedQuery::<EntryColumn>::parse("officer:worf Klingon");
        assert_eq!(q.refusal(), None);
        let (sql, params) = build_where(&q);
        assert_eq!(
            sql,
            " WHERE author = ? AND \
             (LOWER(COALESCE(author, '')) LIKE ? OR LOWER(COALESCE(body, '')) LIKE ?)"
        );
        assert_eq!(params, ["worf", "%klingon%", "%klingon%"]);
        assert_eq!(crate::sort::default_order::<log::Entry>(), "seq DESC");
    }

    /// What a table does not have is refused by name: a range, a flag,
    /// and qmd's predicates on a table qmd does not index.
    #[test]
    fn a_table_refuses_the_keys_it_lacks() {
        for q in ["before:2025-01-01", "is:document", "qmd:tea", "author:worf"] {
            let why = ParsedQuery::<EntryColumn>::parse(q).refusal();
            let key = q.split(':').next().unwrap();
            assert!(
                why.as_deref()
                    .is_some_and(|w| w.contains(&format!("`{key}:`"))),
                "{q}: {why:?}"
            );
        }
    }

    /// `before:` and `after:` compare the UTC twin of `created_at`, the
    /// column the grid sorts by, with the bound normalized to UTC.
    #[test]
    fn before_and_after_bound_the_utc_stamp() {
        let (sql, params) = build_where(&parse_query(
            "after:2025-01-01T00:00:00Z before:2025-02-01T00:00:00Z",
        ));
        assert_eq!(sql, " WHERE created_at_utc < ? AND created_at_utc > ?");
        assert_eq!(params.len(), 2);
        assert!(params[0].starts_with("2025-02-01"), "{params:?}");
        assert!(params[1].starts_with("2025-01-01"), "{params:?}");
    }

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

    /// `author:*` keeps the rows with an author and `-author:*` the rows
    /// with none, an empty one included; nothing is bound.
    #[test]
    fn a_star_is_any_value_and_its_negation_none() {
        let (sql, params) = build_where(&parse_query("project:* -channel:*"));
        assert_eq!(
            sql,
            " WHERE (project IS NOT NULL AND project != '') AND (channel IS NULL OR channel = '')"
        );
        assert!(params.is_empty(), "{params:?}");
    }

    /// What a row's `source_id` is,
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

    /// A person key reads the attached search terms: a handle by its one
    /// value, anything else in part, `*` any term of its kinds; negated, the
    /// rows without one.
    #[test]
    fn a_terms_key_is_a_clause_over_the_attached_terms() {
        let (sql, params) = build_where(&parse_query("from:Ann@Example.com"));
        assert_eq!(
            sql,
            " WHERE uuid IN (SELECT r.uuid FROM search_terms.terms t JOIN search_terms.rows r \
             ON r.row_id = t.row_id WHERE t.kind IN (3, 11) AND t.val_id IN \
             (SELECT val_id FROM search_terms.vals WHERE value = ? COLLATE NOCASE \
             OR value = ? COLLATE NOCASE))"
        );
        assert_eq!(params, ["email:ann@example.com", "Ann@Example.com"]);

        let (sql, params) = build_where(&parse_query(r#"author:"Data""#));
        assert!(
            sql.ends_with(
                "WHERE value = ? COLLATE NOCASE OR value IN (SELECT handle FROM \
                 search_terms.names WHERE name = ? COLLATE NOCASE)))"
            ),
            "{sql}"
        );
        assert_eq!(params, ["Data", "Data"]);

        let mut q = parse_query("with:contact:c-1");
        q.terms[0].handles = Some(vec!["email:a@b.c".into(), "tel:+1555".into()]);
        let (sql, params) = build_where(&q);
        assert!(sql.ends_with("WHERE value IN (?, ?)))"), "{sql}");
        assert_eq!(params, ["email:a@b.c", "tel:+1555"]);

        let (sql, params) = build_where(&parse_query("-author:riker"));
        assert!(sql.starts_with(" WHERE uuid NOT IN ("), "{sql}");
        assert!(sql.contains("LOWER(value) LIKE ?"), "{sql}");
        assert!(
            sql.contains("search_terms.names WHERE LOWER(name) LIKE ?"),
            "{sql}"
        );
        assert_eq!(params, ["%riker%", "%riker%"]);

        let (sql, params) = build_where(&parse_query("recipient:*"));
        assert!(sql.contains("t.kind IN (6, 7, 9))"), "{sql}");
        assert!(params.is_empty());
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
