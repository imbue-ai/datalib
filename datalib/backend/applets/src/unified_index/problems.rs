//! `GET /problems?q=…`: the index's `problems` table as a typed table
//! the viewer draws — every error and warning a step recorded about a
//! record, with the document it belongs to as a link. The Manage
//! screen's errors/warnings cell opens this, filtered to one source.

use axum::extract::{Query, State};
use axum::Json;
use datalib_columns::{Chip, ChipKind, ColumnSpec, ColumnType, Identity};
use datalib_problems::{Outcome, ProblemRow, ScopeKind, Severity};
use datalib_unified_index::problems::{parse, ProblemsQuery};
use serde::{Deserialize, Serialize};

use super::columns::Sources;
use super::Index;

#[derive(Debug, Deserialize)]
pub struct Params {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

/// One row as the viewer draws it: the stored row, with the enums as
/// their words, the severity as a coloured chip, and the source
/// resolved to its configured name.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemView {
    pub problem_uuid: String,
    pub severity: Vec<Chip>,
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
            severity: vec![chip],
            source_ref: sources.identity(&row.source_id),
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

pub fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("severity", "Severity", ColumnType::Chips).describe(
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
    ]
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub columns: Vec<ColumnSpec>,
    /// The field that identifies a row, for the viewer.
    pub row_key: &'static str,
    pub rows: Vec<ProblemView>,
    pub total: usize,
    /// Filters the grammar refused, and anything the read could not
    /// do. The viewer shows them; the rows are what the rest matched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

pub async fn handler(State(s): State<Index>, Query(p): Query<Params>) -> Json<Response> {
    let query: ProblemsQuery = parse(p.q.as_deref().unwrap_or(""));
    let limit = p.limit.unwrap_or(1_000).min(100_000);
    let mut errors = query.errors.clone();
    let rows = match s.repo.problems(&query, limit).await {
        Ok(rows) => rows,
        Err(e) => {
            let msg = format!("problems: {e}");
            eprintln!("{msg}");
            errors.push(msg);
            Vec::new()
        }
    };
    let sources = Sources::read(&s.root);
    let rows: Vec<ProblemView> = rows
        .into_iter()
        .map(|r| ProblemView::of(r, &sources))
        .collect();
    Json(Response {
        columns: columns(),
        row_key: "problem_uuid",
        total: rows.len(),
        rows,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::{Problem, Reason, Scope, Stage};

    /// Every spec names a key the row serializes, and every serialized
    /// key has a spec: a renamed field would otherwise draw an empty
    /// column and nothing would complain.
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
        let specs: std::collections::BTreeSet<&str> =
            specs.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(keys, specs);
        assert_eq!(view.markdown_uuid.as_deref(), Some("md-1"));
        assert_eq!(view.severity[0].kind, ChipKind::Warning);
        assert_eq!(
            view.source_ref.label, "slack",
            "an unconfigured source shows its id"
        );
    }
}
