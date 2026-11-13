//! `GET /problems?q=…`: the index's `problems` table as a typed table
//! the viewer draws — every error and warning a step recorded about a
//! record, with the document it belongs to as a link. The Manage
//! screen's errors/warnings cell opens this, filtered to one source. It
//! reads the way `/search` does: a page at a time, in any order, and
//! grouped on the server (`/problems/groups`). A row a person muted is
//! still listed, marked, and `muted:` keeps or leaves out those
//! (`mutes.rs`).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use std::collections::HashMap;

use datalib_columns::{
    Chip, ChipKind, ColumnSearch, ColumnSpec, ColumnType, DocumentLink, Identity, KeyValues,
    RowsSpec, SearchKeySpec, ValueSuggestion,
};
use datalib_problem_mutes::MuteRow;
use datalib_problems::{Outcome, ProblemRow, ProblemRowColumn, ScopeKind, Severity};
use datalib_unified_document_index::group::Within;
use datalib_unified_document_index::problems::{Ids, ProblemColumn, ProblemsQuery};
use datalib_unified_document_index::repo::LocatedProblem;
use datalib_unified_document_index::sort::Sort;
use datalib_unified_document_index::view;
use serde::{Deserialize, Serialize};

use super::columns::{
    counted, document_identity, free_text_of, keys_of, searchable, value_source, words_holding,
    Sources, ValueSource, ValuesParams,
};
use super::mutes::{self, MutedFilter};
use super::{grouping, results, Index};

/// As `/search` takes them.
#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub q: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// `changed_at_utc:desc,severity_chip`, by the columns' ids.
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
    /// The reason in words a person reads: `Not Found` for `not_found`.
    pub reason_label: String,
    /// What a problem with no document is about, in words: the listing,
    /// configured entry or raw record its scope key names.
    pub about: Option<String>,
    pub field: Option<String>,
    pub rule: Option<String>,
    pub sample: String,
    /// The document this is about — its scope's, or its item's — which
    /// the viewer opens on click.
    pub markdown_uuid: Option<String>,
    /// That document as its chip, by its title.
    pub document_ref: Option<Identity>,
    pub item_uuid: Option<String>,
    pub scope_kind: &'static str,
    pub scope_key: String,
    pub path: Option<String>,
    pub first_seen_at_utc: String,
    pub changed_at_utc: String,
    pub render_version: Option<i64>,
    /// `yes` or `no`, the value `muted:` names.
    pub muted: &'static str,
    /// A muted row's chip, who muted it and when on hover; empty for the rest.
    pub muted_chip: Vec<Chip>,
}

impl ProblemView {
    fn of(located: LocatedProblem, sources: &Sources, mutes: &HashMap<String, MuteRow>) -> Self {
        let LocatedProblem {
            row,
            markdown_uuid,
            document_title,
        } = located;
        let chip = Chip {
            kind: match row.severity {
                Severity::Error => ChipKind::Error,
                Severity::Warning => ChipKind::Warning,
                Severity::Info => ChipKind::Metric,
            },
            text: severity_word(row.severity).to_string(),
            title: format!(
                "{}: {}",
                row.severity.as_str(),
                match row.outcome {
                    Outcome::Dropped => "it is not in the mirror, or what is there is stale",
                    Outcome::Nulled => "it is in the mirror with a field left empty",
                    Outcome::Ok => "nothing was lost",
                }
            ),
        };
        let about = match row.scope_kind {
            ScopeKind::Markdown => None,
            ScopeKind::Entity => Some(
                datalib_etl::download_problems::about(&row.scope_key)
                    .or_else(|| datalib_etl_render::grid_index::about(&row.scope_key))
                    .unwrap_or_else(|| row.scope_key.clone()),
            ),
        };
        let source_ref = sources.identity(&row.source_id);
        let document_ref = markdown_uuid
            .as_deref()
            .map(|doc| document_identity(doc, document_title.as_deref(), &source_ref));
        let mute = mutes.get(&row.problem_uuid);
        ProblemView {
            muted: if mute.is_some() { "yes" } else { "no" },
            muted_chip: mute.map(mutes::chip).into_iter().collect(),
            problem_uuid: row.problem_uuid,
            severity: row.severity.as_str(),
            severity_chip: vec![chip],
            source_ref,
            source_id: row.source_id,
            stage: row.stage.as_str(),
            outcome: row.outcome.as_str(),
            reason: row.reason.as_str(),
            reason_label: words(row.reason.as_str()),
            about,
            field: row.field,
            rule: row.rule,
            sample: row.sample,
            markdown_uuid,
            document_ref,
            item_uuid: row.item_uuid,
            scope_kind: row.scope_kind.as_str(),
            scope_key: row.scope_key,
            path: row.path,
            first_seen_at_utc: row.first_seen_at_utc,
            changed_at_utc: row.changed_at_utc,
            render_version: row.render_version,
        }
    }
}

/// How a severity reads on its chip: short and loud, as a log level does.
fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "ERROR",
        Severity::Warning => "WARN",
        Severity::Info => "INFO",
    }
}

/// A snake_case word as a person reads it: `not_found` is `Not Found`.
fn words(snake: &str) -> String {
    snake
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    /// A person muted it: still listed, and counted nowhere.
    pub muted: bool,
}

impl DocProblem {
    pub fn of(row: ProblemRow, muted: bool) -> Self {
        DocProblem {
            muted,
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

/// A problem opens its document at the section the record has; one
/// about a raw entity with no row opens nothing.
pub fn rows_spec() -> RowsSpec {
    RowsSpec {
        row_key: ProblemRowColumn::ProblemUuid.as_str(),
        document: DocumentLink {
            fields: &["markdown_uuid"],
            anchor: ProblemRowColumn::ItemUuid.as_str(),
            link: None,
        },
        free_text: free_text_of::<ProblemRow>(),
    }
}

pub fn columns() -> Vec<ColumnSpec> {
    let mut columns = searchable::<ProblemColumn>(vec![
        ColumnSpec::new("severity_chip", "Severity", ColumnType::Chips)
            .describe(
                "ERROR: something failed. WARN: something is missing or degraded for a reason \
                 you can act on — access, a configured entry, a limit. INFO: a finding, nothing \
                 lost. Hover a chip for what became of the data.",
            )
            .width(90),
        // Second and wide: what went wrong is what a person opened this for.
        ColumnSpec::new("sample", "Detail", ColumnType::Text)
            .describe(
                "What went wrong: the first 80 characters of the offending value, or, for a \
                 listing, phase or configured entry, the download's explanation whole.",
            )
            .width(420),
        ColumnSpec::new("document_ref", "Document", ColumnType::Identity)
            .describe(
                "The document the record belongs to, by its title. Empty when there is none: a \
                 listing, a configured entry, or a record that never reached the mirror — About \
                 says which.",
            )
            .width(200),
        ColumnSpec::new("about", "About", ColumnType::Text).describe(
            "What a problem with no document is about: a listing or phase the download \
             could not do, a configured entry, a raw record, a render store the index could \
             not read.",
        ),
        ColumnSpec::new("reason_label", "Reason", ColumnType::Text)
            .describe("Why, in a word or two: Not Found, Access Denied, Coercion Failed.")
            .width(130),
        // The Manage screen opens one source's problems: the same name on
        // every line says nothing.
        ColumnSpec::new("source_ref", "Source", ColumnType::Identity).hide_when_uniform(),
        ColumnSpec::new("first_seen_at_utc", "First seen", ColumnType::Timestamp),
        ColumnSpec::new("changed_at_utc", "Last changed", ColumnType::Timestamp),
        ColumnSpec::new("stage", "Stage", ColumnType::Text)
            .describe(
                "Which step noticed it: fetch (downloading), parse (reading the stored payload), \
                 render (projecting it), grid_row (building the index row). The fix is in a \
                 different place for each.",
            )
            .hidden(),
        ColumnSpec::new("field", "Field", ColumnType::Text)
            .describe("The field of the record that was lost or left empty, where it was one.")
            .hidden(),
        ColumnSpec::new("outcome", "Outcome", ColumnType::Text).hidden(),
        ColumnSpec::new("rule", "Rule", ColumnType::Text)
            .describe("The deliberate lossy rule that fired, when one did.")
            .hidden(),
        ColumnSpec::new("markdown_uuid", "Document id", ColumnType::Text).hidden(),
        ColumnSpec::new("item_uuid", "Item", ColumnType::Text)
            .describe("The grid row the record has, or would have had.")
            .hidden(),
        ColumnSpec::new("scope_kind", "Scope", ColumnType::Text)
            .describe("What clears this row when reprocessed: the document, or the raw entity.")
            .hidden(),
        ColumnSpec::new("scope_key", "Scope key", ColumnType::Text).hidden(),
        ColumnSpec::new("path", "Path", ColumnType::Text)
            .describe("Where in the stored payload, as a JSON pointer.")
            .hidden(),
        ColumnSpec::new("render_version", "Render version", ColumnType::Count).hidden(),
        ColumnSpec::new("problem_uuid", "Problem id", ColumnType::Text).hidden(),
    ]);
    // Third, beside what went wrong: a muted row is one a person has
    // already seen. Not a column of the index, so its term is read in
    // `mutes::split_muted` and the rows cannot be ordered by it.
    columns.insert(
        2,
        ColumnSpec {
            search: Some(ColumnSearch {
                key: mutes::KEY.into(),
                field: "muted".into(),
                partial: false,
            }),
            ..ColumnSpec::new("muted_chip", "Muted", ColumnType::Chips)
                .describe(
                    "MUTED: someone has seen this and asked not to be told again. It stays here \
                     and counts toward nothing (not the Dashboard's Needs you, not a source's \
                     count) until it changes or is unmuted. Right-click rows to mute or unmute \
                     them; muted:no leaves the muted ones out.",
                )
                .width(80)
                .unsorted()
        },
    );
    columns
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub columns: Vec<ColumnSpec>,
    #[serde(flatten)]
    pub rows_spec: RowsSpec,
    pub rows: Vec<ProblemView>,
    /// The rows can be muted: `POST /problems/mute`.
    pub mutes: bool,
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
        mutes: true,
        total: 0,
        next_offset: None,
        at: None,
        errors: Vec::new(),
    };
    // A term the search cannot read is the answer, not dropped: the rest
    // of the query alone would match more than was asked for.
    let (query, muted) = match read_query(p.q.as_deref().unwrap_or("")) {
        Ok(read) => read,
        Err(why) => {
            out.errors.push(why);
            return Json(out);
        }
    };
    let mutes = match read_mutes(&s, muted, &mut out.errors).await {
        Some(mutes) => mutes,
        None => return Json(out),
    };
    let ids = mutes::ids_for(muted, &mutes);
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
        ids: &ids,
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
                .map(|r| ProblemView::of(r, &sources, &mutes))
                .collect();
            out.total = page.total;
            out.next_offset = page.next_offset;
            out.at = page.at;
        }
        Err(e) => {
            let msg = format!("problems: {e}");
            tracing::error!(error = %e, "could not read the problems");
            out.errors.push(msg);
        }
    }
    Json(out)
}

struct PageSpec<'a> {
    ids: &'a Ids,
    sort: &'a [Sort<ProblemRowColumn>],
    within: &'a [Within<ProblemRowColumn>],
    offset: usize,
    limit: usize,
    through: Option<&'a str>,
}

struct Page {
    rows: Vec<LocatedProblem>,
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
        .problem_keys(query, spec.ids, spec.sort, spec.within)
        .await
        .map_err(|e| e.to_string())?;
    let list: Vec<results::Entry> = listing
        .uuids
        .into_iter()
        .map(|uuid| results::Entry::new(uuid, None))
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

/// The keys the problems grid's search bar offers as a person types.
pub async fn keys_handler() -> Json<Vec<SearchKeySpec>> {
    let mut keys = keys_of::<ProblemRow>();
    keys.push(SearchKeySpec {
        key: mutes::KEY,
        aliases: &[],
        values: KeyValues::Words {
            words: mutes::WORDS.to_vec(),
        },
        partial: false,
    });
    Json(keys)
}

/// The query with its `muted:` terms read out, or why it cannot be read.
fn read_query(q: &str) -> Result<(ProblemsQuery, Option<MutedFilter>), String> {
    let (rest, muted) = mutes::split_muted(q)?;
    let query = ProblemsQuery::parse(&rest);
    match query.refusal() {
        Some(why) => Err(why),
        None => Ok((query, muted)),
    }
}

/// What is muted, or `None` when that could not be read and the answer
/// depends on it. When only the marks do, the rows go out unmarked and
/// `errors` says why.
async fn read_mutes(
    s: &Index,
    muted: Option<MutedFilter>,
    errors: &mut Vec<String>,
) -> Option<HashMap<String, MuteRow>> {
    match s.mutes.all().await {
        Ok(mutes) => Some(mutes),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "could not read the mutes");
            errors.push(format!("could not read which problems are muted: {e:#}"));
            muted.is_none().then(HashMap::new)
        }
    }
}

/// `GET /problems/values?key=…&typed=…&q=…`: what the search bar
/// suggests for one key's value.
pub async fn values_handler(
    State(s): State<Index>,
    Query(p): Query<ValuesParams>,
) -> Result<Json<Vec<ValueSuggestion>>, (StatusCode, String)> {
    if p.key == mutes::KEY {
        return Ok(Json(words_holding(mutes::WORDS, &p.typed)));
    }
    let rest = mutes::split_muted(&p.q).map_or_else(|_| p.q.clone(), |(rest, _)| rest);
    let suggested = match value_source::<ProblemRow>(&p.key) {
        ValueSource::Words(words) => Ok(words_holding(&words, &p.typed)),
        ValueSource::Column(column) => s
            .repo
            .problem_value_counts(&ProblemsQuery::parse(&rest), column, &p.typed)
            .await
            .map(counted),
        ValueSource::Nothing => Ok(Vec::new()),
    };
    suggested.map(Json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("suggest values: {e}"),
        )
    })
}

/// `GET /problems/groups?q=…&by=…` — the groups the problems fall into,
/// each with its count, in no order: the grid orders them.
pub async fn groups_handler(
    State(s): State<Index>,
    Query(p): Query<GroupParams>,
) -> Json<GroupsResponse> {
    let mut out = GroupsResponse::default();
    let (query, muted) = match read_query(p.q.as_deref().unwrap_or("")) {
        Ok(read) => read,
        Err(why) => {
            out.errors.push(why);
            return Json(out);
        }
    };
    let Some(mutes) = read_mutes(&s, muted, &mut out.errors).await else {
        return Json(out);
    };
    let by = match grouping::parse_by::<ProblemColumn>(&p.by) {
        Ok(by) => by,
        Err(e) => {
            out.errors.push(e);
            return Json(out);
        }
    };
    match s
        .repo
        .problem_groups(&query, &mutes::ids_for(muted, &mutes), &by)
        .await
    {
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
                    sample: ProblemView::of(g.sample, &sources, &mutes),
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
        datalib_etl_render::grid_index::seal(
            &datalib_etl_render::grid_index::WriteLock::for_index(pool.clone()),
        )
        .await
        .unwrap();
        pool.close().await;
    }

    fn seen(mut row: ProblemRow, at: &str) -> ProblemRow {
        row.first_seen_at_utc = at.to_string();
        row.changed_at_utc = at.to_string();
        row
    }

    /// Three problems, last changed newest first in this order: a warning
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
            "source:enterprise nonsense:x",
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
            "last changed first"
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

    /// A muted row stays in the table, marked with who muted it, and
    /// `muted:` keeps or leaves out the muted ones in the rows and in the
    /// groups alike. Unmuted, it is a row like the rest again; a root
    /// nobody muted anything in never gets the store.
    #[tokio::test]
    async fn a_muted_row_stays_marked_and_muted_terms_keep_or_leave_it() {
        let tmp = tempfile::tempdir().unwrap();
        let [warning, parse, worf] = three();
        index_problems(tmp.path(), &[warning.clone(), parse.clone(), worf.clone()]).await;
        let s = super::super::tests::index_over(tmp.path()).await;
        let store = datalib_problem_mutes::store_path(tmp.path());

        let all = ask(&s, q("")).await;
        assert!(all.mutes);
        assert!(all
            .rows
            .iter()
            .all(|r| r.muted == "no" && r.muted_chip.is_empty()));
        assert!(!store.exists(), "reading made the store");

        let body = |ids: &[&ProblemRow], muted| mutes::MuteBody {
            problem_uuids: ids.iter().map(|r| r.problem_uuid.clone()).collect(),
            muted,
            agent: None,
        };
        let _ = mutes::handler(State(s.clone()), Json(body(&[&parse, &worf], true)))
            .await
            .unwrap();
        assert!(store.exists());

        let all = ask(&s, q("")).await;
        assert_eq!(all.total, 3, "a muted row is still a row");
        let marked: Vec<&str> = all
            .rows
            .iter()
            .filter(|r| r.muted == "yes")
            .map(|r| r.problem_uuid.as_str())
            .collect();
        let mut want = [parse.problem_uuid.as_str(), worf.problem_uuid.as_str()];
        want.sort();
        let mut got = marked.clone();
        got.sort();
        assert_eq!(got, want);
        let chip = &all
            .rows
            .iter()
            .find(|r| r.muted == "yes")
            .unwrap()
            .muted_chip[0];
        assert_eq!(chip.text, "MUTED");
        assert!(chip.title.starts_with("Muted by you at "), "{}", chip.title);

        assert_eq!(
            ids(&ask(&s, q("muted:no")).await),
            [warning.problem_uuid.as_str()]
        );
        assert_eq!(
            ids(&ask(&s, q("-muted:yes")).await),
            [warning.problem_uuid.as_str()]
        );
        assert_eq!(
            ids(&ask(&s, q("muted:yes source:defiant")).await),
            [worf.problem_uuid.as_str()]
        );
        let refused = ask(&s, q("muted:sometimes")).await;
        assert!(refused.rows.is_empty() && refused.errors.len() == 1);

        let groups = groups_handler(
            State(s.clone()),
            Query(GroupParams {
                q: Some("muted:no".into()),
                by: "severity_chip".into(),
            }),
        )
        .await
        .0;
        assert!(groups.errors.is_empty(), "{:?}", groups.errors);
        let counted: Vec<(Option<String>, u64)> = groups
            .groups
            .iter()
            .map(|g| (g.values[0].clone(), g.count))
            .collect();
        assert_eq!(counted, [(Some("warning".to_string()), 1)]);

        let _ = mutes::handler(State(s.clone()), Json(body(&[&parse], false)))
            .await
            .unwrap();
        let mut left = ids(&ask(&s, q("muted:no")).await)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        left.sort();
        let mut want = vec![warning.problem_uuid.clone(), parse.problem_uuid.clone()];
        want.sort();
        assert_eq!(left, want);

        let empty = mutes::handler(State(s.clone()), Json(body(&[], true))).await;
        assert_eq!(empty.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_search_bar_offers_muted_with_its_two_words() {
        let keys = keys_handler().await.0;
        let muted = keys.iter().find(|k| k.key == "muted").unwrap();
        assert_eq!(
            muted.values,
            KeyValues::Words {
                words: vec!["yes", "no"]
            }
        );
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
            search("document_ref"),
            Some(("doc".into(), "scope_key".into()))
        );
        assert_eq!(
            search("reason_label"),
            Some(("reason".into(), "reason".into()))
        );
        assert_eq!(search("sample"), None);
    }

    /// One source's problems repeat its name on every line, so the
    /// problems grid hides Source while they do; the search grid, whose
    /// `source:` searches read that column, keeps it.
    #[test]
    fn only_the_problems_source_hides_when_it_repeats() {
        let hides = |specs: Vec<ColumnSpec>| {
            specs
                .into_iter()
                .filter(|c| c.hide_when_uniform)
                .map(|c| c.field)
                .collect::<Vec<_>>()
        };
        assert_eq!(hides(columns()), ["source_ref"]);
        assert!(hides(super::super::columns::columns()).is_empty());
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
        assert_eq!(DocProblem::of(dropped, false).item_uuid, None);
        let nulled = ProblemRow::new(
            "slack",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("u-1"),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "x"),
            Some(3),
        );
        assert_eq!(
            DocProblem::of(nulled, false).item_uuid.as_deref(),
            Some("u-1")
        );
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
        let located = LocatedProblem {
            row,
            markdown_uuid: Some("md-1".into()),
            document_title: Some("Captain's log".into()),
        };
        let view = ProblemView::of(located, &Sources::default(), &HashMap::new());
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
        let doc = view.document_ref.as_ref().unwrap();
        assert_eq!(
            (doc.label.as_str(), doc.entity.as_deref()),
            ("Captain's log", Some("datalib:doc/md-1"))
        );
        assert_eq!(view.severity_chip[0].kind, ChipKind::Warning);
        assert_eq!(view.severity_chip[0].text, "WARN");
        assert_eq!(view.reason_label, "Coercion Failed");
        assert_eq!(
            view.source_ref.label, "slack",
            "an unconfigured source shows its id"
        );
    }
}
