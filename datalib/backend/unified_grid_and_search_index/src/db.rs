//! Pure helpers used by `IndexRepo` implementations: dialect-agnostic
//! WHERE-builder and the [`ChatMeta`] row shape the impl returns. All SQL goes through `sqlx` against
//! [`crate::dolt_repo::DoltRepo`].

use crate::group::like_pattern;
use crate::query::{Field, FilterTerm, ParsedQuery};
use crate::terms_keys::{self, Cells, TermsKey, TermsValue};
use datalib_query::table::{Column, FreeText, SearchTable};
use datalib_schema::grid_rows::{GridRowColumn, INDEXES as GRID_ROWS_INDEXES};
use datalib_schema::providers::Provider;
use datalib_schema::search_terms::{SearchTermKind, FROM_THE_DOCUMENT};

/// The source id datalib's own rows are filed under — the storage
/// reports, which describe a source's mirror rather than belonging to
/// it. Same spelling as their `provider` tag, because that tag is what
/// the filter and the Source column both test.
pub fn datalib_source_id() -> &'static str {
    Provider::Datalib.as_str()
}

/// Per-conversation header data read from `grid_rows`. The chat preview
/// renders the markdown body verbatim and pulls the page header from here —
/// no markdown parsing.
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
    /// The picture the document is, where it is one: a
    /// `datalib:photo/…` name, which has no page on disk.
    pub image: Option<String>,
}

/// A term's value that stands for any value at all: `author:*`.
pub const ANY_VALUE: &str = "*";

/// How a set of rows a term keeps enters the `WHERE`. SQLite takes any set
/// to be 25 rows and reads the table for each, where a key's index holds
/// what a search reads: beside a key an index starts with, a set is tested
/// (`+`), and only without one does a search start from it.
fn set_of<C: Column>(q: &ParsedQuery<C>) -> &'static str {
    let starts_at_a_key = q.terms.iter().any(|t| match t.field {
        Field::Column(key) => !t.negate && t.value != ANY_VALUE && starts_an_index(key.column),
        Field::At => !t.negate,
        Field::Before
        | Field::After
        | Field::Is(_)
        | Field::Terms(_)
        | Field::Text
        | Field::Names => false,
    });
    if starts_at_a_key {
        "+"
    } else {
        ""
    }
}

fn starts_an_index<C: Column>(column: C) -> bool {
    GRID_ROWS_INDEXES.iter().any(|(table, ddl)| {
        *table == <C::Table as SearchTable>::TABLE
            && ddl
                .rsplit('(')
                .next()
                .and_then(|columns| columns.split([',', ')']).next())
                == Some(column.as_str())
    })
}

/// Build the SQL `WHERE` clause (with a leading space) and the matching
/// parameter list for a parsed query's terms, and for its free text when
/// the table matches free text with `LIKE`; a ranked table's free text is
/// the search index's.
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
    let pk = <C::Table as SearchTable>::PRIMARY_KEY.as_str();
    let set = set_of(q);
    for term in &q.terms {
        let (clause, bound) = match term.field {
            Field::Terms(key) => terms_clause(set, key, term),
            Field::Names => names_clause(set, term),
            Field::Text => text_clause(set, pk, term),
            Field::At => crate::at_node::clause(&term.value, term.arms, term.negate),
            Field::Before | Field::After | Field::Is(_) | Field::Column(_) => {
                (String::new(), Vec::new())
            }
        };
        if !clause.is_empty() {
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
        // A uuid is stored one way (`grid_index::uuids_one_way`), so it is
        // typed that way too; a uuid key takes it at the end of a slug or a
        // link as well.
        let uuid = key
            .uuid
            .then(|| datalib_uuid_text::at_end(&term.value))
            .flatten()
            .or_else(|| datalib_uuid_text::canonical(&term.value));
        // `upstream_id` is as the source wrote it, which a uuid's term is
        // not.
        if let (Some(uuid), true) = (&uuid, col == GridRowColumn::UpstreamId.as_str()) {
            let not = if term.negate { "NOT " } else { "" };
            clauses.push(format!(
                "rowid {not}IN (SELECT t.row_id FROM terms t WHERE t.kind = {} \
                 AND t.val_id IN (SELECT val_id FROM vals WHERE value = ?))",
                SearchTermKind::UpstreamId.code()
            ));
            params.push(uuid.clone());
            continue;
        }
        let value = uuid.unwrap_or_else(|| term.value.clone());
        if term.negate {
            // Nullable columns: NULL would pass `col != ?` as unknown
            // and be dropped, which surprises users who didn't ask to
            // exclude unset values. Explicitly keep nulls.
            clauses.push(format!("({col} IS NULL OR {col} != ?)"));
        } else {
            clauses.push(format!("{col} = ?"));
        }
        params.push(value);
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

/// The index a narrowing to documents reads alone, which holds each
/// document's id and source beside the flag. Fetching those from the
/// table instead took seconds a search on a cold disk: one random read
/// for each of a library's documents.
pub const DOCUMENTS_INDEX: &str = "grid_rows_by_is_document";

/// What [`matching_sql`] reads of each row kept: the row, its document and
/// whether it is the document's own; or each document once.
pub const KEPT_ROWS: &str = "r.uuid AS uuid, d.uuid AS markdown_uuid, r.is_document AS is_document";
pub const KEPT_DOCUMENTS: &str = "DISTINCT d.uuid AS markdown_uuid";

/// The `columns` of every row of a document that `q`'s terms keep, and
/// the values to bind: what a search narrows by before it ranks. The
/// filter comes out of `grid_rows`' indexes as row ids, which every one
/// of them ends in, and the row and its document are read from `rows`
/// by those integers, never from the table. Names [`DOCUMENTS_INDEX`]
/// when the terms are `is:document` and `source:` alone, so the plan
/// cannot drift back to the table.
pub fn matching_sql(
    grid_rows: &str,
    columns: &str,
    q: &ParsedQuery<GridRowColumn>,
) -> (String, Vec<String>) {
    let (where_sql, params) = build_where(q);
    let indexed = if documents_and_sources_only(q) {
        format!(" INDEXED BY {DOCUMENTS_INDEX}")
    } else {
        String::new()
    };
    let kept = if where_sql.is_empty() {
        String::new()
    } else {
        format!(" WHERE r.row_id IN (SELECT rowid FROM {grid_rows}{indexed}{where_sql})")
    };
    (
        format!("SELECT {columns} FROM rows r JOIN rows d ON d.row_id = r.doc_row_id{kept}"),
        params,
    )
}

fn documents_and_sources_only(q: &ParsedQuery<GridRowColumn>) -> bool {
    q.documents() == Some(true)
        && q.terms.iter().all(|t| match t.field {
            Field::Is(column) => column == GridRowColumn::IsDocument,
            Field::Column(key) => key.column == GridRowColumn::SourceId,
            Field::Before
            | Field::After
            | Field::Terms(_)
            | Field::Text
            | Field::Names
            | Field::At => false,
        })
}

/// `text` as an FTS5 phrase over the search index's titles and bodies: its
/// words in order, each whole.
pub fn fts5_phrase(text: &str) -> String {
    format!("{{title body}} : (\"{}\")", text.replace('"', "\"\""))
}

/// The rows whose text holds `term`'s words in order, or with `-`, does
/// not: a row a message is when its message holds them, a document's own
/// row (whose uuid is its document's) when any piece of it does. A row
/// the search index does not hold holds no words.
fn text_clause<C>(set: &str, pk: &str, term: &FilterTerm<C>) -> (String, Vec<String>) {
    let not = if term.negate { "NOT " } else { "" };
    let phrase = fts5_phrase(&term.value);
    (
        format!(
            "{set}{pk} {not}IN (SELECT s.uuid FROM search_fts f \
             JOIN search_sections s ON s.section_id = f.rowid \
             WHERE search_fts MATCH ? AND s.uuid IS NOT NULL \
             UNION SELECT d.markdown_uuid FROM search_fts f \
             JOIN search_sections s ON s.section_id = f.rowid \
             JOIN search_documents d ON d.doc_id = s.doc_id WHERE search_fts MATCH ?)"
        ),
        vec![phrase.clone(), phrase],
    )
}

/// The rows that name `term`'s id or handle, whole and case-blind, in any
/// of their terms or, as one of [`FROM_THE_DOCUMENT`], in their
/// document's; or with `-`, do not.
/// Both spellings are asked for, as a person key asks: the handle, and the
/// address as typed, which an author with no handle is filed under.
fn names_clause<C>(set: &str, term: &FilterTerm<C>) -> (String, Vec<String>) {
    let not = if term.negate { "NOT " } else { "" };
    let mut values = vec![term.value.clone()];
    if let Some(id) = crate::query::identifier(&term.value).filter(|id| *id != term.value) {
        values.push(id);
    }
    let any = vec!["value = ? COLLATE NOCASE"; values.len()].join(" OR ");
    let named = format!("t.val_id IN (SELECT val_id FROM vals WHERE {any})");
    (
        format!(
            "{set}rowid {not}IN (SELECT t.row_id FROM terms t WHERE {named} \
             UNION SELECT r.row_id FROM terms t JOIN rows r ON r.doc_row_id = t.row_id \
             WHERE t.kind IN ({}) AND {named})",
            codes(FROM_THE_DOCUMENT)
        ),
        values.iter().chain(&values).cloned().collect(),
    )
}

/// `kinds` as their codes, a list SQL reads. They are the enum's own
/// numbers, so they are written in.
fn codes(kinds: &[SearchTermKind]) -> String {
    kinds
        .iter()
        .map(|k| k.code().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The rows whose search terms of `key`'s kinds hold `value`, as one
/// clause over `pk` and the values it binds. The kinds are the enum's own
/// codes, so they are written in; the value is bound.
fn terms_clause<C>(set: &str, key: &TermsKey, term: &FilterTerm<C>) -> (String, Vec<String>) {
    if let Some(clause) = cell_clause(key, term) {
        return (clause, Vec::new());
    }
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
            // `rating:>9` names no value, so matches no row.
            if any.is_empty() && by_name.is_none() {
                any.push("0".to_string());
            }
            if let Some(name) = by_name {
                any.push(
                    "value IN (SELECT handle FROM names WHERE name = ? COLLATE NOCASE)".to_string(),
                );
                bound.push(name);
            }
            (
                format!(
                    " AND t.val_id IN (SELECT val_id FROM vals WHERE {})",
                    any.join(" OR ")
                ),
                bound,
            )
        }
        // `vals_fts` tokenizes the value as the terms were, so the phrase
        // is the words in order, each whole.
        TermsValue::Words { text, by_name } => {
            let phrase = format!("\"{}\"", text.replace('"', "\"\""));
            let names = if by_name {
                " OR t.val_id IN (SELECT val_id FROM vals WHERE value IN \
                     (SELECT handle FROM names WHERE name = ? COLLATE NOCASE))"
                    .to_string()
            } else {
                String::new()
            };
            let bound = if by_name {
                vec![phrase, text]
            } else {
                vec![phrase]
            };
            (
                format!(
                    " AND (t.val_id IN (SELECT rowid FROM vals_fts \
                     WHERE vals_fts MATCH ?){names})"
                ),
                bound,
            )
        }
        TermsValue::Partial { text, by_name } => {
            let (held, needle) = vals_holding("t.val_id", &text);
            let names = if by_name {
                " OR t.val_id IN (SELECT val_id FROM vals WHERE value IN \
                     (SELECT handle FROM names WHERE LOWER(name) LIKE ? ESCAPE '\\'))"
                    .to_string()
            } else {
                String::new()
            };
            let bound = if by_name {
                vec![needle, like_pattern(&text)]
            } else {
                vec![needle]
            };
            (format!(" AND ({held}{names})"), bound)
        }
        // A contact, or the owner, is its handles, read before the query
        // runs; none read is no row (an empty array), and the applet refuses
        // one it could not read before it gets here.
        TermsValue::Contact(_) | TermsValue::Me => {
            let handles = term.handles.as_deref().unwrap_or_default();
            (
                " AND t.val_id IN (SELECT val_id FROM vals \
                     WHERE value IN (SELECT value FROM json_each(?)))"
                    .to_string(),
                vec![serde_json::Value::from(handles.to_vec()).to_string()],
            )
        }
    };
    let not = if term.negate { "NOT " } else { "" };
    (
        format!(
            "{set}rowid {not}IN (SELECT t.row_id FROM terms t WHERE t.kind IN ({}){matched})",
            codes(&key.kinds())
        ),
        bound,
    )
}

/// The values in `vals` holding `typed`, case-blind, as a clause over
/// `val_id` and the one value it binds. `vals_tri` looks up a run of
/// three characters or more; anything shorter reads every value.
pub(crate) fn vals_holding(val_id: &str, typed: &str) -> (String, String) {
    if typed.chars().count() >= 3 {
        (
            format!("{val_id} IN (SELECT rowid FROM vals_tri WHERE vals_tri MATCH ?)"),
            format!("\"{}\"", typed.replace('"', "\"\"")),
        )
    } else {
        (
            format!("{val_id} IN (SELECT val_id FROM vals WHERE LOWER(value) LIKE ? ESCAPE '\\')"),
            like_pattern(typed),
        )
    }
}

/// A value that names an H3 cell, on a key that reads cells: the rows
/// whose location is in its ranges (datalib/backend/h3/README.md). The
/// bounds are integers we computed, so they go into the SQL as written.
fn cell_clause<C>(key: &TermsKey, term: &FilterTerm<C>) -> Option<String> {
    let cell = datalib_h3::parse(&term.value)?;
    let ranges = match key.cells {
        Cells::AsTerm => return None,
        Cells::Inside => vec![datalib_h3::finest_range(cell)],
        Cells::Near => datalib_h3::neighbourhood_ranges(cell),
    };
    let col = GridRowColumn::H3.as_str();
    let any = ranges
        .iter()
        .map(|r| format!("{col} BETWEEN {} AND {}", r.start(), r.end()))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some(if term.negate {
        format!("({col} IS NULL OR NOT ({any}))")
    } else {
        // Seven ORed ranges, which nothing tells SQLite keep very few rows:
        // it would walk the newest-first index instead.
        format!("likelihood(({any}), 0.001)")
    })
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
    /// and the ranked search's predicates on a table that is not ranked.
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

    /// `before:` and `after:` compare the UTC twin of `touched_at`, the
    /// column the grid sorts by, with the bound normalized to UTC.
    #[test]
    fn before_and_after_bound_the_utc_stamp() {
        let (sql, params) = build_where(&parse_query(
            "after:2025-01-01T00:00:00Z before:2025-02-01T00:00:00Z",
        ));
        assert_eq!(sql, " WHERE touched_at_utc < ? AND touched_at_utc > ?");
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

    /// What a row's `source_id` is, storage rows filed under datalib
    /// included, is decided once when the row is written
    /// (`grid_rows::filed_under`); the filter only compares, so the
    /// `(source_id, …)` index can serve it.
    #[test]
    fn source_id_filter_is_equality_on_the_column() {
        let (sql, params) = build_where(&parse_query("source:slack_work"));
        assert_eq!(sql, " WHERE source_id = ?");
        assert_eq!(params, vec!["slack_work"]);

        let (sql, params) = build_where(&parse_query("-source:datalib"));
        assert_eq!(sql, " WHERE (source_id IS NULL OR source_id != ?)");
        assert_eq!(params, vec!["datalib"]);
    }

    /// A person key reads the attached search terms: a handle by its one
    /// value, a quoted name by its words and the handles seen under it,
    /// anything else in part, `*` any term of its kinds; negated, the rows
    /// without one.
    #[test]
    fn a_terms_key_is_a_clause_over_the_terms() {
        let (sql, params) = build_where(&parse_query("from:Ann@Example.com"));
        assert_eq!(
            sql,
            " WHERE rowid IN (SELECT t.row_id FROM terms t WHERE t.kind IN (3, 11) \
             AND t.val_id IN (SELECT val_id FROM vals WHERE value = ? COLLATE NOCASE \
             OR value = ? COLLATE NOCASE))"
        );
        assert_eq!(params, ["email:ann@example.com", "Ann@Example.com"]);

        let (sql, params) = build_where(&parse_query(r#"author:"Data""#));
        assert!(
            sql.ends_with(
                "AND (t.val_id IN (SELECT rowid FROM vals_fts WHERE vals_fts \
                 MATCH ?) OR t.val_id IN (SELECT val_id FROM vals WHERE value IN \
                 (SELECT handle FROM names WHERE name = ? COLLATE NOCASE))))"
            ),
            "{sql}"
        );
        assert_eq!(params, ["\"Data\"", "Data"]);

        let mut q = parse_query("with:contact:c-1");
        q.terms[0].handles = Some(vec!["email:a@b.c".into(), "tel:+1555".into()]);
        let (sql, params) = build_where(&q);
        assert!(
            sql.ends_with("WHERE value IN (SELECT value FROM json_each(?))))"),
            "{sql}"
        );
        assert_eq!(params, [r#"["email:a@b.c","tel:+1555"]"#]);

        let q = parse_query("from:me").with_me_handles(&["me:self".into(), "email:a@b.c".into()]);
        let (sql, params) = build_where(&q);
        assert!(
            sql.ends_with("WHERE value IN (SELECT value FROM json_each(?))))"),
            "{sql}"
        );
        assert_eq!(params, [r#"["me:self","email:a@b.c"]"#]);

        let (sql, params) = build_where(&parse_query("-author:riker"));
        assert!(sql.starts_with(" WHERE rowid NOT IN ("), "{sql}");
        assert!(sql.contains("vals_tri MATCH ?"), "{sql}");
        assert!(sql.contains("names WHERE LOWER(name) LIKE ?"), "{sql}");
        assert_eq!(params, ["\"riker\"", "%riker%"]);

        // Too short for a trigram: every value is read.
        let (sql, params) = build_where(&parse_query("from:ri"));
        assert!(sql.contains("LOWER(value) LIKE ?"), "{sql}");
        assert!(!sql.contains("vals_tri"), "{sql}");
        assert_eq!(params, ["%ri%", "%ri%"]);

        let (sql, params) = build_where(&parse_query("recipient:*"));
        assert!(sql.contains("t.kind IN (6, 7, 9))"), "{sql}");
        assert!(params.is_empty());

        let (sql, params) = build_where(&parse_query("rating:>=4"));
        assert!(sql.contains("t.kind IN (20)"), "{sql}");
        assert_eq!(params, ["4", "5"]);

        let (sql, params) = build_where(&parse_query("rating:>5"));
        assert!(sql.contains("FROM vals WHERE 0)"), "{sql}");
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

    /// A uuid typed without dashes (a Notion link's) or in capitals found
    /// nothing stored with them. It is stored one way, so it is compared
    /// that way, once: in the column, or for `id:` in its search term,
    /// since the column keeps the source's spelling. Any other id as typed.
    #[test]
    fn a_uuid_value_is_compared_as_it_is_stored() {
        let (sql, params) = build_where(&parse_query("account:1701D000abcd4e00A0000000000000ef"));
        assert_eq!(sql, " WHERE account = ?");
        assert_eq!(params, ["1701d000-abcd-4e00-a000-0000000000ef"]);
        let (sql, params) = build_where(&parse_query("id:1701D000abcd4e00A0000000000000ef"));
        assert_eq!(
            sql,
            " WHERE rowid IN (SELECT t.row_id FROM terms t WHERE t.kind = 15 \
             AND t.val_id IN (SELECT val_id FROM vals WHERE value = ?))"
        );
        assert_eq!(params, ["1701d000-abcd-4e00-a000-0000000000ef"]);
        let (sql, _) = build_where(&parse_query("-id:1701d000-abcd-4e00-a000-0000000000ef"));
        assert!(sql.starts_with(" WHERE rowid NOT IN ("), "{sql}");
        let (sql, params) = build_where(&parse_query("id:M-ncc1701-0042"));
        assert_eq!(sql, " WHERE upstream_id = ?");
        assert_eq!(params, ["M-ncc1701-0042"]);
    }

    /// A key that holds a uuid takes it at the end of a page's link, as
    /// Notion writes it, without dashes.
    #[test]
    fn a_uuid_key_reads_its_uuid_off_a_link() {
        let (sql, params) = build_where(&parse_query(
            "notion_page:https://www.notion.so/Away-Team-1701d000abcd4e00a0000000000000ef?pvs=4",
        ));
        assert_eq!(sql, " WHERE notion_page_uuid = ?");
        assert_eq!(params, ["1701d000-abcd-4e00-a000-0000000000ef"]);
    }

    /// A pasted id reaches a document's rows through the document's own
    /// row, which alone holds its id and title; every other kind only
    /// names the row that holds it.
    #[test]
    fn a_pasted_id_reaches_a_documents_rows_through_its_row() {
        let (sql, params) = build_where(&parse_query("00000000-0000-4000-8000-0000000000e0"));
        assert_eq!(
            sql,
            " WHERE rowid IN (SELECT t.row_id FROM terms t WHERE t.val_id IN \
             (SELECT val_id FROM vals WHERE value = ? COLLATE NOCASE) \
             UNION SELECT r.row_id FROM terms t JOIN rows r ON r.doc_row_id = t.row_id \
             WHERE t.kind IN (1, 4) AND t.val_id IN \
             (SELECT val_id FROM vals WHERE value = ? COLLATE NOCASE))"
        );
        assert_eq!(params.len(), 2);
    }
}
