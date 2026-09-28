//! `GET /problems?q=…`: the index's `problems` table as a typed table
//! the viewer draws — every error and warning a step recorded about a
//! record, with the document it belongs to as a link. The Manage
//! screen's errors/warnings cell opens this, filtered to one source. It
//! reads the way `/search` does: a page at a time, in any order, and
//! grouped on the server (`/problems/groups`).

use axum::extract::{Query, State};
use axum::Json;
use datalib_columns::{Chip, ChipKind, ColumnSpec, ColumnType, DocumentLink, Identity, RowsSpec};
use datalib_problems::{Outcome, ProblemRow, ProblemRowColumn, ScopeKind, Severity};
use datalib_unified_index::group::Within;
use datalib_unified_index::problems::{ProblemColumn, ProblemsQuery};
use datalib_unified_index::sort::Sort;
use datalib_unified_index::view;
use serde::{Deserialize, Serialize};

use super::columns::{free_text_of, searchable, Sources};
use super::{grouping, results, Index};

/// As `/search` takes them.
#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub q: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// `last_seen_at_utc:desc,severity_chip`, by the columns' ids.
    pub sort: Option<String>,
    /// Stretch the page to reach this problem.
    pub through: Option<String>,
    /// `[["severity_chip","error"]]`: one group's problems.
    pub within: Option<String>,
}

/// One row as the viewer draws it: the stored row, with the enums as
/// their words, the severity also as a coloured chip, and the source
/// resolved to its configured name. A column a key filters keeps its
/// stored value under the stored column's name, which is where the
/// viewer reads the value a term names (`ColumnSearch::field`).
#[derive(Debug, Clone, Serialize)]
pub struct ProblemView {
    pub problem_uuid: String,
    pub severity: &'static str,
    pub severity_chip: Vec<Chip>,
    pub source_id: String,
    pub source_ref: Identity,
    pub stage: &'static str,
    pub outcome: &'static str,
    pub reason: &'static str,
    pub field: Option<String>,
    pub rule: Option<String>,
    pub sample: String,
    /// The document this is about, when the scope is a document: the
    /// viewer opens it on click.
    pub markdown_uuid: Option<String>,
    pub item_uuid: Option<String>,
    pub scope_kind: &'static str,
    pub scope_key: String,
    pub path: Option<String>,
    pub first_seen_at_utc: String,
    pub last_seen_at_utc: String,
    pub render_version: Option<i64>,
}

impl ProblemView {
    fn of(row: ProblemRow, sources: &Sources) -> Self {
        let chip = Chip {
            kind: match row.severity {
                Severity::Error => ChipKind::Error,
                Severity::Warning => ChipKind::Warning,
                Severity::Info => ChipKind::Metric,
            },
            text: row.severity.as_str().to_string(),
            title: format!(
                "{}: the record was {}",
                row.severity.as_str(),
                match row.outcome {
                    Outcome::Dropped => "dropped",
                    Outcome::Nulled => "kept with a field nulled",
                    Outcome::Ok => "kept intact",
                }
            ),
        };
        let markdown_uuid = (row.scope_kind == ScopeKind::Markdown).then(|| row.scope_key.clone());
        ProblemView {
            problem_uuid: row.problem_uuid,
            severity: row.severity.as_str(),
            severity_chip: vec![chip],
            source_ref: sources.identity(&row.source_id),
            source_id: row.source_id,
            stage: row.stage.as_str(),
            outcome: row.outcome.as_str(),
            reason: row.reason.as_str(),
            field: row.field,
            rule: row.rule,
            sample: row.sample,
            markdown_uuid,
            item_uuid: row.item_uuid,
            scope_kind: row.scope_kind.as_str(),
            scope_key: row.scope_key,
            path: row.path,
            first_seen_at_utc: row.first_seen_at_utc,
            last_seen_at_utc: row.last_seen_at_utc,
            render_version: row.render_version,
        }
    }
}

/// One problem as the document view shows it above the body: enough
/// to say what went wrong and to jump to the section, no more.
#[derive(Debug, Clone, Serialize)]
pub struct DocProblem {
    pub problem_uuid: String,
    pub severity: &'static str,
    pub stage: &'static str,
    pub outcome: &'static str,
    pub reason: &'static str,
    pub field: Option<String>,
    pub rule: Option<String>,
    pub sample: String,
    /// The section in the body this is about, when the record survived
    /// as a row; a dropped record has no section to jump to.
    pub item_uuid: Option<String>,
    pub first_seen_at_utc: String,
}

impl DocProblem {
    pub fn of(row: ProblemRow) -> Self {
        DocProblem {
            problem_uuid: row.problem_uuid,
            severity: row.severity.as_str(),
            stage: row.stage.as_str(),
            outcome: row.outcome.as_str(),
            reason: row.reason.as_str(),
            field: row.field,
            rule: row.rule,
            sample: row.sample,
            item_uuid: (row.outcome != Outcome::Dropped)
                .then_some(row.item_uuid)
                .flatten(),
            first_seen_at_utc: row.first_seen_at_utc,
        }
    }
}

/// Errors first, then warnings, then findings; within a severity the
/// order the store returned.
pub fn sort_for_banner(rows: &mut [ProblemRow]) {
    rows.sort_by_key(|r| match r.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
}

/// A problem about a document opens it at the section the record has;
/// one about a raw entity, before any document, opens nothing.
pub fn rows_spec() -> RowsSpec {
    RowsSpec {
        row_key: ProblemRowColumn::ProblemUuid.as_str(),
        document: DocumentLink {
            fields: &["markdown_uuid"],
            anchor: ProblemRowColumn::ItemUuid.as_str(),
        },
        free_text: free_text_of::<ProblemRow>(),
    }
}

pub fn columns() -> Vec<ColumnSpec> {
    searchable::<ProblemColumn>(vec![
        ColumnSpec::new("severity_chip", "Severity", ColumnType::Chips).describe(
            "error: the record was dropped. warning: it was kept with something lost. \
             info: a finding, nothing lost.",
        ),
        ColumnSpec::new("source_ref", "Source", ColumnType::Identity),
        ColumnSpec::new("stage", "Stage", ColumnType::Text).describe(
            "Which step noticed it: fetch (downloading), parse (reading the stored payload), \
             render (projecting it), grid_row (building the index row). The fix is in a \
             different place for each.",
        ),
        ColumnSpec::new("reason", "Reason", ColumnType::Text),
        ColumnSpec::new("field", "Field", ColumnType::Text),
        ColumnSpec::new("sample", "Sample", ColumnType::Text)
            .describe("The first 80 characters of the offending value."),
        ColumnSpec::new("markdown_uuid", "Document", ColumnType::MarkdownUuid)
            .describe("The document the record belongs to. Empty when the failure happened before that was known."),
        ColumnSpec::new("outcome", "Outcome", ColumnType::Text).hidden(),
        ColumnSpec::new("rule", "Rule", ColumnType::Text)
            .describe("The deliberate lossy rule that fired, when one did.")
            .hidden(),
        ColumnSpec::new("item_uuid", "Item", ColumnType::Text)
            .describe("The grid row the record has, or would have had.")
            .hidden(),
        ColumnSpec::new("first_seen_at_utc", "First seen", ColumnType::Timestamp),
        ColumnSpec::new("last_seen_at_utc", "Last seen", ColumnType::Timestamp),
        ColumnSpec::new("scope_kind", "Scope", ColumnType::Text)
            .describe("What clears this row when reprocessed: the document, or the raw entity.")
            .hidden(),
        ColumnSpec::new("scope_key", "Scope key", ColumnType::Text).hidden(),
        ColumnSpec::new("path", "Path", ColumnType::Text)
            .describe("Where in the stored payload, as a JSON pointer.")
            .hidden(),
        ColumnSpec::new("render_version", "Render version", ColumnType::Count).hidden(),
        ColumnSpec::new("problem_uuid", "Problem id", ColumnType::Text).hidden(),
    ])
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub columns: Vec<ColumnSpec>,
    #[serde(flatten)]
    pub rows_spec: RowsSpec,
    pub rows: Vec<ProblemView>,
    /// How many problems the query matches, across every page.
    pub total: usize,
    /// Where the next page starts; `None` on the last.
    pub next_offset: Option<usize>,
    /// The index commit the list was read at.
    pub at: Option<String>,
    /// Filters the grammar refused, and anything the read could not
    /// do. The viewer shows them; a refused filter leaves no rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

pub async fn handler(State(s): State<Index>, Query(p): Query<Params>) -> Json<Response> {
    let mut out = Response {
        columns: columns(),
        rows_spec: rows_spec(),
        rows: Vec::new(),
        total: 0,
        next_offset: None,
        at: None,
        errors: Vec::new(),
    };
    let query = ProblemsQuery::parse(p.q.as_deref().unwrap_or(""));
    // A term the search cannot read is the answer, not dropped: the rest
    // of the query alone would match more than was asked for.
    if let Some(why) = query.refusal() {
        out.errors.push(why);
        return Json(out);
    }
    let sort = match p
        .sort
        .as_deref()
        .map(view::order::<ProblemColumn>)
        .transpose()
    {
        Ok(sort) => sort.unwrap_or_default(),
        Err(e) => {
            out.errors.push(format!("{e}; showing the default order"));
            Vec::new()
        }
    };
    let within = match p
        .within
        .as_deref()
        .map(grouping::parse_within::<ProblemColumn>)
        .transpose()
    {
        Ok(within) => within.unwrap_or_default(),
        Err(e) => {
            out.errors.push(e);
            return Json(out);
        }
    };
    let spec = PageSpec {
        sort: &sort,
        within: &within,
        offset: p.offset.unwrap_or(0),
        limit: p.limit.unwrap_or(1_000).min(results::MAX_PAGE),
        through: p.through.as_deref(),
    };
    match page(&s, &query, spec).await {
        Ok(page) => {
            let sources = Sources::read(&s.root);
            out.rows = page
                .rows
                .into_iter()
                .map(|r| ProblemView::of(r, &sources))
                .collect();
            out.total = page.total;
            out.next_offset = page.next_offset;
            out.at = page.at;
        }
        Err(e) => {
            let msg = format!("problems: {e}");
            eprintln!("{msg}");
            out.errors.push(msg);
        }
    }
    Json(out)
}

struct PageSpec<'a> {
    sort: &'a [Sort<ProblemRowColumn>],
    within: &'a [Within<ProblemRowColumn>],
    offset: usize,
    limit: usize,
    through: Option<&'a str>,
}

struct Page {
    rows: Vec<ProblemRow>,
    total: usize,
    next_offset: Option<usize>,
    at: Option<String>,
}

/// One page of the problems `query` matches. Listed per request, not
/// cached as a search is: there is no qmd ranking to save, and the list
/// is of problems, not of every row in the root.
async fn page(s: &Index, query: &ProblemsQuery, spec: PageSpec<'_>) -> Result<Page, String> {
    let listing = s
        .repo
        .problem_keys(query, spec.sort, spec.within)
        .await
        .map_err(|e| e.to_string())?;
    let list: Vec<results::Entry> = listing
        .uuids
        .into_iter()
        .map(|uuid| results::Entry { uuid, hit: None })
        .collect();
    let limit = results::reaching(&list, spec.offset, spec.limit, spec.through);
    let (entries, next_offset) = results::page(&list, spec.offset, limit);
    let keys: Vec<String> = entries.iter().map(|e| e.uuid.clone()).collect();
    Ok(Page {
        rows: s
            .repo
            .problems_by_keys(&keys)
            .await
            .map_err(|e| e.to_string())?,
        total: list.len(),
        next_offset,
        at: listing.at,
    })
}

#[derive(Debug, Deserialize)]
pub struct GroupParams {
    pub q: Option<String>,
    /// The columns to group by, outermost first: `source_ref,severity_chip`.
    pub by: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GroupsResponse {
    pub groups: Vec<GroupOut>,
    /// More groups than one answer carries: the rest are left out.
    pub truncated: bool,
    pub at: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GroupOut {
    /// The group's value in each grouped column, as `within=` spells it
    /// back.
    pub values: Vec<Option<String>>,
    pub count: u64,
    /// The group's most recently seen problem, which its labels are read
    /// from.
    pub sample: ProblemView,
}

/// `GET /problems/groups?q=…&by=…` — the groups the problems fall into,
/// each with its count, in no order: the grid orders them.
pub async fn groups_handler(
    State(s): State<Index>,
    Query(p): Query<GroupParams>,
) -> Json<GroupsResponse> {
    let mut out = GroupsResponse::default();
    let query = ProblemsQuery::parse(p.q.as_deref().unwrap_or(""));
    if let Some(why) = query.refusal() {
        out.errors.push(why);
        return Json(out);
    }
    let by = match grouping::parse_by::<ProblemColumn>(&p.by) {
        Ok(by) => by,
        Err(e) => {
            out.errors.push(e);
            return Json(out);
        }
    };
    match s.repo.problem_groups(&query, &by).await {
        Ok(grouping) => {
            let sources = Sources::read(&s.root);
            out.truncated = grouping.truncated;
            out.at = grouping.at;
            out.groups = grouping
                .groups
                .into_iter()
                .map(|g| GroupOut {
                    values: g.values,
                    count: g.count,
                    sample: ProblemView::of(g.sample, &sources),
                })
                .collect();
        }
        Err(e) => out.errors.push(format!("group the problems: {e}")),
    }
    Json(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::{Problem, Reason, Scope, Stage};

    /// Problems committed to the root's grid index. The step copies a
    /// source's problems in from its render store; this writes the rows
    /// that copy would.
    async fn index_problems(root: &std::path::Path, problems: &[ProblemRow]) {
        use datalib_table::BulkUpsertable;

        let pool = datalib_etl_render::grid_index::open_index(
            &datalib_runtime::layout::grid_index_db(root),
        )
        .await
        .unwrap();
        let columns: Vec<&str> = std::iter::once(ProblemRow::ID_COLUMN)
            .chain(ProblemRow::TYPED_COLUMNS.iter().copied())
            .collect();
        let placeholders = vec!["?"; columns.len()].join(", ");
        let sql = format!(
            "INSERT INTO problems ({}) VALUES ({placeholders})",
            columns.join(", ")
        );
        for problem in problems {
            problem
                .bind_into(sqlx::query(sqlx::AssertSqlSafe(sql.clone())))
                .execute(&pool)
                .await
                .unwrap();
        }
        datalib_etl::doltlite_raw::commit_run(&pool, "problems")
            .await
            .unwrap();
        pool.close().await;
    }

    fn seen(mut row: ProblemRow, at: &str) -> ProblemRow {
        row.first_seen_at_utc = at.to_string();
        row.last_seen_at_utc = at.to_string();
        row
    }

    /// Three problems, last seen newest first in this order: a warning
    /// on the Enterprise's log, and two dropped records, one each.
    fn three() -> [ProblemRow; 3] {
        let problem = |source: &str, stage, doc, outcome, p| {
            ProblemRow::new(
                source,
                stage,
                Scope::Markdown(doc),
                Some(doc),
                outcome,
                p,
                None,
            )
        };
        [
            seen(
                problem(
                    "enterprise",
                    Stage::GridRow,
                    "log-1",
                    Outcome::Nulled,
                    Problem::field("created_at", Reason::CoercionFailed, "stardate 41153.7"),
                ),
                "2026-01-03T00:00:00Z",
            ),
            seen(
                problem(
                    "enterprise",
                    Stage::Parse,
                    "log-2",
                    Outcome::Dropped,
                    Problem::field("uuid", Reason::NoIdentity, ""),
                ),
                "2026-01-02T00:00:00Z",
            ),
            seen(
                problem(
                    "defiant",
                    Stage::GridRow,
                    "log-3",
                    Outcome::Dropped,
                    Problem::field("author", Reason::CoercionFailed, "Worf"),
                ),
                "2026-01-01T00:00:00Z",
            ),
        ]
    }

    async fn ask(s: &Index, params: Params) -> Response {
        handler(State(s.clone()), Query(params)).await.0
    }

    fn q(q: &str) -> Params {
        Params {
            q: Some(q.to_string()),
            ..Params::default()
        }
    }

    fn ids(r: &Response) -> Vec<&str> {
        r.rows.iter().map(|v| v.problem_uuid.as_str()).collect()
    }

    /// A term the problems search cannot read is refused with no rows,
    /// not dropped: the rest of the query alone matches more than was
    /// asked for, and the table would read as the answer.
    #[tokio::test]
    async fn a_term_the_search_cannot_read_is_refused_not_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        index_problems(tmp.path(), &three()[..1]).await;
        let s = super::super::tests::index_over(tmp.path()).await;

        let all = ask(&s, q("")).await;
        assert_eq!(all.rows.len(), 1, "{:?}", all.errors);
        for text in [
            "nonsense:x",
            "severity:catastrophic",
            "source_id:enterprise nonsense:x",
        ] {
            let r = ask(&s, q(text)).await;
            assert!(r.rows.is_empty(), "{text}: {} rows", r.rows.len());
            assert_eq!(r.errors.len(), 1, "{text}: {:?}", r.errors);
        }
    }

    /// The table reads the way the search grid does: a page at a time,
    /// stretched to reach a row, in any order, one group at a time, with
    /// free text a substring of the sample.
    #[tokio::test]
    async fn problems_page_sort_and_narrow_like_the_search() {
        let tmp = tempfile::tempdir().unwrap();
        let [warning, parse, worf] = three();
        index_problems(tmp.path(), &[warning.clone(), parse.clone(), worf.clone()]).await;
        let s = super::super::tests::index_over(tmp.path()).await;

        let first = ask(
            &s,
            Params {
                limit: Some(1),
                ..Params::default()
            },
        )
        .await;
        assert_eq!(
            ids(&first),
            [warning.problem_uuid.as_str()],
            "last seen first"
        );
        assert_eq!((first.total, first.next_offset), (3, Some(1)));
        assert!(first.at.is_some());

        let reaching = ask(
            &s,
            Params {
                limit: Some(1),
                through: Some(worf.problem_uuid.clone()),
                ..Params::default()
            },
        )
        .await;
        assert_eq!(reaching.rows.len(), 3);
        assert_eq!(reaching.next_offset, None);

        // The warning is the newest, so first by default; by severity it
        // follows both errors.
        let by_severity = ask(
            &s,
            Params {
                sort: Some("severity_chip:asc".into()),
                ..Params::default()
            },
        )
        .await;
        assert_eq!(by_severity.rows.len(), 3);
        assert_eq!(by_severity.rows[2].problem_uuid, warning.problem_uuid);

        let errors = ask(
            &s,
            Params {
                within: Some(r#"[["severity_chip","error"]]"#.into()),
                ..Params::default()
            },
        )
        .await;
        let mut got = ids(&errors);
        got.sort();
        let mut want = [parse.problem_uuid.as_str(), worf.problem_uuid.as_str()];
        want.sort();
        assert_eq!(got, want);

        assert_eq!(ids(&ask(&s, q("worf")).await), [worf.problem_uuid.as_str()]);
        assert_eq!(
            ids(&ask(&s, q("source:defiant")).await),
            [worf.problem_uuid.as_str()]
        );
    }

    /// Every group comes with its count, and its sample is the problem
    /// in it seen last.
    #[tokio::test]
    async fn problems_group_on_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let [warning, parse, worf] = three();
        index_problems(tmp.path(), &[warning, parse.clone(), worf]).await;
        let s = super::super::tests::index_over(tmp.path()).await;

        let params = GroupParams {
            q: None,
            by: "severity_chip".into(),
        };
        let r = groups_handler(State(s.clone()), Query(params)).await.0;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let mut groups: Vec<(Option<String>, u64, String)> = r
            .groups
            .iter()
            .map(|g| (g.values[0].clone(), g.count, g.sample.problem_uuid.clone()))
            .collect();
        groups.sort();
        assert_eq!(groups[0].0.as_deref(), Some("error"));
        assert_eq!(groups[0].1, 2);
        assert_eq!(groups[0].2, parse.problem_uuid, "the newer of the two");
        assert_eq!((groups[1].0.as_deref(), groups[1].1), (Some("warning"), 1));
    }

    #[test]
    fn a_column_offers_the_key_that_filters_it() {
        let search = |id: &str| {
            columns()
                .into_iter()
                .find(|c| c.field == id)
                .and_then(|c| c.search)
                .map(|s| (s.key, s.field))
        };
        assert_eq!(
            search("severity_chip"),
            Some(("severity".into(), "severity".into()))
        );
        assert_eq!(
            search("markdown_uuid"),
            Some(("doc".into(), "scope_key".into()))
        );
        assert_eq!(search("sample"), None);
    }

    /// Every spec names a key the row serializes, and every serialized
    /// key has a spec: a renamed field would otherwise draw an empty
    /// column and nothing would complain.
    /// A dropped record has no section in the body, so its banner line
    /// must not offer a jump that lands nowhere.
    #[test]
    fn a_dropped_record_offers_no_section_to_jump_to() {
        let dropped = ProblemRow::new(
            "slack",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("u-1"),
            Outcome::Dropped,
            Problem::field("uuid", Reason::NoIdentity, ""),
            Some(3),
        );
        assert_eq!(DocProblem::of(dropped).item_uuid, None);
        let nulled = ProblemRow::new(
            "slack",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("u-1"),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "x"),
            Some(3),
        );
        assert_eq!(DocProblem::of(nulled).item_uuid.as_deref(), Some("u-1"));
    }

    #[test]
    fn every_column_is_a_row_key_and_back() {
        let row = ProblemRow::new(
            "slack",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("u-1"),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "yesterday"),
            Some(3),
        );
        let view = ProblemView::of(row, &Sources::default());
        let json = serde_json::to_value(&view).unwrap();
        let keys: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let specs = columns();
        let fields: std::collections::BTreeSet<&str> =
            specs.iter().map(|c| c.field.as_str()).collect();
        // Beside the columns, a row carries only the values their keys name.
        let named: std::collections::BTreeSet<&str> = specs
            .iter()
            .filter_map(|c| c.search.as_ref())
            .map(|s| s.field.as_str())
            .collect();
        assert!(fields.is_subset(&keys), "{:?}", fields.difference(&keys));
        let extra: Vec<&&str> = keys.difference(&fields).collect();
        assert!(extra.iter().all(|k| named.contains(**k)), "{extra:?}");
        assert!(specs
            .iter()
            .filter_map(|c| c.search.as_ref())
            .all(|s| keys.contains(s.field.as_str())));
        assert_eq!(view.markdown_uuid.as_deref(), Some("md-1"));
        assert_eq!(view.severity_chip[0].kind, ChipKind::Warning);
        assert_eq!(
            view.source_ref.label, "slack",
            "an unconfigured source shows its id"
        );
    }
}
