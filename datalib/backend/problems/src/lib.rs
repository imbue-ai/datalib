//! The `problems` table: one row per thing a step could not fully do to
//! one record, kept in the store the step owns, where a reader finds it.
//! Every closed vocabulary here is an enum: the row binds them as text
//! and a reader parses them back, never comparing the column to a
//! literal. Where the rows are, and what reads them: `etl/README.md`
//! §"Problems stay in the step that found them".

mod recorded;
pub use recorded::{log_recorded, note_recorded};

use anyhow::{Context, Result};
use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// The closed sets a problem row is described in. They are the step
/// protocol's own, since a step's `problem` line spells them too
/// (`datalib_step_protocol::problems`).
pub use datalib_step_protocol::problems::{Outcome, Reason, ScopeKind, Severity, Stage};

/// The stages a step other than the download and the render records in a
/// store of its own, each that step's alone. `unified_grid_and_search_index`
/// copies each one's rows apart from the render store's, so neither copy
/// erases the other's.
pub const OWN_STORE_STAGES: [Stage; 2] = [Stage::TimeSeries, Stage::Occurrences];

/// The index every per-record lookup (`scope_kind = ? AND scope_key = ?`)
/// names with `INDEXED BY`. A store creates it as it opens and keeps it
/// out of its recorded shape, so adding it re-renders nothing.
pub const BY_SCOPE_INDEX: &str = "problems_by_scope";
pub const BY_SCOPE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS problems_by_scope ON problems (scope_kind, scope_key)";

/// The scope key of a row the grid index records itself: a source's
/// render store it could not read. Every other row in the index is a copy.
pub const INDEX_OWN_SCOPE_KEY: &str = "render_store";

/// The `problems` table's shape: the top of the framework's shared ladder
/// (`datalib_etl::doltlite_raw::SHARED_LADDER`), which every store holding
/// the table records as its `shared_schema_version`. A reader of a store's
/// problems reads them only from a store at this version.
pub const SHAPE_VERSION: u32 = 1;

/// Which step's Manage count a row in the grid index is in: each step
/// counts only the rows it found itself (`problem_mutes/README.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CountedBy {
    /// The source's download: a fetch-stage row.
    Download,
    /// The source's render: every other row of the source.
    Render,
    /// The source's `render_time_series`: a time-series-stage row.
    TimeSeries,
    /// The source's `render_occurrences`: an occurrences-stage row.
    Occurrences,
    /// The grid index, about a render store it could not read.
    Index,
}

impl CountedBy {
    pub fn of(stage: Stage, scope_kind: ScopeKind, scope_key: &str) -> Self {
        match (stage, scope_kind) {
            (_, ScopeKind::Entity) if scope_key == INDEX_OWN_SCOPE_KEY => CountedBy::Index,
            (Stage::Fetch, _) => CountedBy::Download,
            (Stage::TimeSeries, _) => CountedBy::TimeSeries,
            (Stage::Occurrences, _) => CountedBy::Occurrences,
            _ => CountedBy::Render,
        }
    }

    /// The stages whose rows are this step's alone; empty for the render,
    /// whose rows are every stage the others leave, and for the index,
    /// whose one row is a render-stage row named by its key.
    pub fn own_stages(self) -> &'static [Stage] {
        match self {
            CountedBy::Download => &[Stage::Fetch],
            CountedBy::TimeSeries => &[Stage::TimeSeries],
            CountedBy::Occurrences => &[Stage::Occurrences],
            CountedBy::Render | CountedBy::Index => &[],
        }
    }
}

/// The metric series a step reports its whole-store problem counts on,
/// one sample per severity under the [`METRIC_LABEL`] label, every run
/// and zero included: the Manage screen reads a missing series as
/// "never counted", not as clean.
pub const METRIC: &str = "problems";
pub const METRIC_LABEL: &str = "severity";

/// The `severity=<word>` label a [`METRIC`] sample carries.
pub fn metric_label(severity: Severity) -> (&'static str, &'static str) {
    (METRIC_LABEL, severity.as_str())
}

/// The severity a run-store label string names, `severity=error` as the
/// store canonicalizes it.
pub fn severity_in_metric_labels(labels: &str) -> Option<Severity> {
    labels
        .split(',')
        .find_map(|kv| {
            kv.strip_prefix(METRIC_LABEL)
                .and_then(|v| v.strip_prefix('='))
        })
        .and_then(Severity::parse)
}

/// The sweep key, typed: what must be reprocessed for this row to be
/// re-evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Markdown(&'a str),
    Entity(&'a str),
}

impl<'a> Scope<'a> {
    pub fn kind(self) -> ScopeKind {
        match self {
            Scope::Markdown(_) => ScopeKind::Markdown,
            Scope::Entity(_) => ScopeKind::Entity,
        }
    }

    pub fn key(self) -> &'a str {
        match self {
            Scope::Markdown(k) | Scope::Entity(k) => k,
        }
    }
}

static STEP_SOURCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Set once by `datalib-step`, from its group: the source every problem
/// this process's step finds is about. A download's writers mint their
/// rows under it, since none of them is handed the source.
pub fn set_step_source(source_id: &str) {
    let _ = STEP_SOURCE.set(source_id.to_string());
}

/// The source [`set_step_source`] named; empty where nothing set it, as
/// in a library's own tests.
pub fn step_source() -> &'static str {
    STEP_SOURCE.get().map_or("", String::as_str)
}

pub fn sample_of(s: &str) -> String {
    const MAX: usize = 80;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// One thing that went wrong, before it is tied to a record and a
/// store: what a renderer or a fetcher hands to [`ProblemRow::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub reason: Reason,
    /// The field this is about; `None` for a record-level problem
    /// (undeserializable, no identity).
    pub field: Option<String>,
    /// Where in the stored payload, as a JSON pointer, when we know it.
    pub path: Option<String>,
    /// The rule that fired, when this was a deliberate lossy rule rather
    /// than a defect. Must be stable across runs —
    /// name it for the rule, not for the value it happened to see.
    pub rule: Option<String>,
    /// First 80 characters of the offending value — see [`sample_of`] —
    /// or, from [`Problem::explained`] and [`Problem::lossy_explained`],
    /// the writer's explanation whole.
    pub sample: String,
    /// `None` takes [`Severity::default_for`] the outcome.
    pub severity: Option<Severity>,
}

impl Problem {
    /// A field-level problem: the field survived as null, or did not
    /// survive at all, and here is what it looked like.
    pub fn field(name: impl Into<String>, reason: Reason, sample: &str) -> Self {
        Self {
            reason,
            field: Some(name.into()),
            path: None,
            rule: None,
            sample: sample_of(sample),
            severity: None,
        }
    }

    pub fn record(reason: Reason, sample: &str) -> Self {
        Self {
            reason,
            field: None,
            path: None,
            rule: None,
            sample: sample_of(sample),
            severity: None,
        }
    }

    /// A problem whose sample is an explanation rather than a value —
    /// about a listing, a phase or a configured entry, or the error a
    /// fetch got — kept whole: cut at 80 characters it stops saying what
    /// happened, since a fetch error spends that much on its URL.
    pub fn explained(reason: Reason, field: Option<String>, explanation: &str) -> Self {
        Self {
            reason,
            field,
            path: None,
            rule: None,
            sample: explanation.to_string(),
            severity: None,
        }
    }

    /// A deliberate lossy rule fired. `rule` names it, so the rows can be
    /// grouped by rule.
    pub fn lossy(rule: impl Into<String>, field: Option<String>, sample: &str) -> Self {
        Self {
            reason: Reason::DeliberateLoss,
            field,
            path: None,
            rule: Some(rule.into()),
            sample: sample_of(sample),
            severity: None,
        }
    }

    /// A deliberate lossy rule that fired on a whole source or record
    /// rather than on one value, so its sample is the explanation, kept
    /// whole as [`Problem::explained`] keeps one.
    pub fn lossy_explained(
        rule: impl Into<String>,
        field: Option<String>,
        explanation: &str,
    ) -> Self {
        Self {
            rule: Some(rule.into()),
            ..Self::explained(Reason::DeliberateLoss, field, explanation)
        }
    }

    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn severity(mut self, severity: Severity) -> Self {
        self.severity = Some(severity);
        self
    }
}

/// One problem, on one record, in one store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "problems",
    primary_key = "problem_uuid",
    // The search bar's keys are on the columns they filter.
    search(
        order = "changed_at_utc desc, problem_uuid asc",
        range = "changed_at_utc"
    )
)]
pub struct ProblemRow {
    /// Minted by `datalib_id::problem_id` from the columns that name
    /// the problem — never from the sample or the stamps — so a re-run
    /// of the same code over the same record mints the same id.
    #[col(sql = "VARCHAR(36)", search = "problem", alias = "problem_uuid")]
    pub problem_uuid: String,
    /// The id of the source that produced this, matching
    /// `markdowns.source_id`.
    #[col(sql = "VARCHAR(64)", search = "source", alias = "source_id")]
    pub source_id: String,
    #[col(sql = "VARCHAR(16)", enum, search)]
    pub stage: Stage,
    #[col(sql = "VARCHAR(16)", enum, search)]
    pub severity: Severity,
    #[col(sql = "VARCHAR(16)", enum, search)]
    pub outcome: Outcome,
    #[col(sql = "VARCHAR(32)", enum, search)]
    pub reason: Reason,
    #[col(sql = "VARCHAR(16)", enum, search = "scope", alias = "scope_kind")]
    pub scope_kind: ScopeKind,
    /// The sweep key: the `markdown_uuid` of the document the record
    /// belongs to, or — when the failure happened before we knew that —
    /// the raw-store entity id.
    #[col(
        sql = "VARCHAR(96)",
        search = "doc",
        alias = "markdown_uuid",
        alias = "scope_key"
    )]
    pub scope_key: String,
    /// The `grid_rows.uuid` the record has, or would have had; `None`
    /// for a problem about the whole document or entity. With a
    /// markdown scope this is the section the document view can scroll
    /// to. On a fetch-stage row it is the row the raw entity is or
    /// belongs to, set only where the render store holds that row.
    #[col(
        sql = "VARCHAR(96)",
        search = "item",
        alias = "item_uuid",
        alias = "uuid"
    )]
    pub item_uuid: Option<String>,
    #[col(sql = "VARCHAR(128)", search, like)]
    pub field: Option<String>,
    #[col(sql = "VARCHAR(255)")]
    pub path: Option<String>,
    #[col(sql = "VARCHAR(128)", search, like)]
    pub rule: Option<String>,
    #[col(sql = "TEXT", like)]
    pub sample: String,
    /// When this problem was first recorded, in UTC. Stamped by the
    /// store that first holds it, not the writer, and carried through
    /// every copy downstream.
    #[col(sql = "VARCHAR(40)")]
    pub first_seen_at_utc: String,
    /// When it last changed: first recorded, or recorded again with a
    /// different severity, outcome, reason or sample. A problem recorded
    /// again unchanged keeps this stamp, so a standing problem changes
    /// nothing in the store. Also stamped by the store.
    #[col(sql = "VARCHAR(40)")]
    pub changed_at_utc: String,
    /// The offset the store's clock was in at `changed_at_utc`.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// The `RENDER_VERSION` of the renderer that recorded it, so a row
    /// left by an older renderer is identifiable. `None` off a fetch.
    #[col(sql = "INT")]
    pub render_version: Option<i64>,
    /// The upstream record the problem is about, as the step that found
    /// it named it: the values a grid row carries under the same names,
    /// which is how a reader finds the row a problem is about.
    #[col(sql = "VARCHAR(64)")]
    pub upstream_entity_kind: Option<String>,
    #[col(sql = "VARCHAR(255)")]
    pub upstream_id: Option<String>,
    #[col(sql = "VARCHAR(255)")]
    pub upstream_account: Option<String>,
}

impl ProblemRow {
    /// A row with its id minted and its stamps left empty for the store
    /// to fill — the store is the only layer that can tell "first seen"
    /// from "seen again".
    pub fn new(
        source_id: &str,
        stage: Stage,
        scope: Scope<'_>,
        item_uuid: Option<&str>,
        outcome: Outcome,
        problem: Problem,
        render_version: Option<u32>,
    ) -> Self {
        let problem_uuid = datalib_id::problem_id(
            source_id,
            stage.as_str(),
            scope.kind().as_str(),
            scope.key(),
            item_uuid,
            problem.field.as_deref(),
            problem.reason.as_str(),
            problem.rule.as_deref(),
        );
        Self {
            problem_uuid,
            source_id: source_id.to_string(),
            stage,
            severity: problem
                .severity
                .unwrap_or_else(|| Severity::default_for(outcome)),
            outcome,
            reason: problem.reason,
            scope_kind: scope.kind(),
            scope_key: scope.key().to_string(),
            item_uuid: item_uuid.map(str::to_string),
            field: problem.field,
            path: problem.path,
            rule: problem.rule,
            sample: problem.sample,
            first_seen_at_utc: String::new(),
            changed_at_utc: String::new(),
            tz_offset: None,
            render_version: render_version.map(i64::from),
            upstream_entity_kind: None,
            upstream_id: None,
            upstream_account: None,
        }
    }

    /// This row as the store keeps it, given the row the store already
    /// holds for the same record, if any: recorded again unchanged, it
    /// keeps every stamp; changed, it keeps when it was first seen and
    /// is stamped `now`.
    pub fn stamped(
        self,
        earlier: Option<&ProblemRow>,
        now_utc: &str,
        tz_offset: Option<&str>,
    ) -> Self {
        let unstamped = |r: &ProblemRow| ProblemRow {
            first_seen_at_utc: String::new(),
            changed_at_utc: String::new(),
            tz_offset: None,
            ..r.clone()
        };
        match earlier {
            Some(e) if unstamped(e) == unstamped(&self) => ProblemRow {
                first_seen_at_utc: e.first_seen_at_utc.clone(),
                changed_at_utc: e.changed_at_utc.clone(),
                tz_offset: e.tz_offset.clone(),
                ..self
            },
            _ => ProblemRow {
                first_seen_at_utc: earlier
                    .map_or_else(|| now_utc.to_string(), |e| e.first_seen_at_utc.clone()),
                changed_at_utc: now_utc.to_string(),
                tz_offset: tz_offset.map(str::to_string),
                ..self
            },
        }
    }

    /// Whether this stamped row says something the one before it did not:
    /// `changed_at_utc` moves only when what a row says does.
    pub fn is_news(&self, earlier: Option<&ProblemRow>) -> bool {
        earlier.is_none_or(|e| e.changed_at_utc != self.changed_at_utc)
    }

    /// Every column, by name, off a `SELECT *`. A vocabulary word this
    /// build does not know is an error, not a guess: the store was
    /// written by a newer build and the caller decides what that means.
    pub fn from_row(r: &sqlx::sqlite::SqliteRow) -> Result<Self> {
        fn word<T: Copy>(
            r: &sqlx::sqlite::SqliteRow,
            col: &str,
            parse: fn(&str) -> Option<T>,
        ) -> Result<T> {
            let s: String = r.try_get(col)?;
            parse(&s).with_context(|| format!("problems.{col}: unknown spelling {s:?}"))
        }
        Ok(Self {
            problem_uuid: r.try_get("problem_uuid")?,
            source_id: r.try_get("source_id")?,
            stage: word(r, "stage", Stage::parse)?,
            severity: word(r, "severity", Severity::parse)?,
            outcome: word(r, "outcome", Outcome::parse)?,
            reason: word(r, "reason", Reason::parse)?,
            scope_kind: word(r, "scope_kind", ScopeKind::parse)?,
            scope_key: r.try_get("scope_key")?,
            item_uuid: r.try_get("item_uuid")?,
            field: r.try_get("field")?,
            path: r.try_get("path")?,
            rule: r.try_get("rule")?,
            sample: r.try_get("sample")?,
            first_seen_at_utc: r.try_get("first_seen_at_utc")?,
            changed_at_utc: r.try_get("changed_at_utc")?,
            tz_offset: r.try_get("tz_offset")?,
            render_version: r.try_get("render_version")?,
            upstream_entity_kind: r.try_get("upstream_entity_kind")?,
            upstream_id: r.try_get("upstream_id")?,
            upstream_account: r.try_get("upstream_account")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(sample: &str) -> ProblemRow {
        ProblemRow::new(
            "",
            Stage::Fetch,
            Scope::Entity("media_blobs:p1#a.mp4"),
            None,
            Outcome::Ok,
            Problem::record(Reason::NotFound, sample),
            None,
        )
    }

    /// An old error logged again on every run read as a new failure each
    /// time. Only a row saying something new is news.
    #[test]
    fn a_row_recorded_again_unchanged_is_not_news() {
        let row = |sample: &str| {
            ProblemRow::new(
                "tng",
                Stage::Fetch,
                Scope::Entity("record"),
                None,
                Outcome::Ok,
                Problem::record(Reason::NotFound, sample),
                None,
            )
        };
        let first = row("gone").stamped(None, "day 1", None);
        assert!(first.is_news(None));
        let again = row("gone").stamped(Some(&first), "day 2", None);
        assert!(!again.is_news(Some(&first)));
        let changed = row("still gone").stamped(Some(&again), "day 3", None);
        assert!(changed.is_news(Some(&again)));
    }

    /// A standing problem re-stamped on every run made a commit, and
    /// re-ran every step downstream, on every sync.
    #[test]
    fn a_problem_recorded_again_unchanged_keeps_its_stamps() {
        let first = missing("not in the export").stamped(None, "t1", Some("+02:00"));
        assert_eq!(
            (
                first.first_seen_at_utc.as_str(),
                first.changed_at_utc.as_str()
            ),
            ("t1", "t1")
        );
        let again = missing("not in the export").stamped(Some(&first), "t2", Some("-07:00"));
        assert_eq!(again, first);

        let changed = missing("would not read").stamped(Some(&first), "t3", Some("-07:00"));
        assert_eq!(
            (
                changed.first_seen_at_utc.as_str(),
                changed.changed_at_utc.as_str(),
                changed.tz_offset.as_deref()
            ),
            ("t1", "t3", Some("-07:00"))
        );
    }

    #[test]
    fn sample_truncates_on_a_char_boundary_and_marks_the_cut() {
        assert_eq!(sample_of("short"), "short");
        // 40 two-byte chars = 80 bytes: exactly at the limit, uncut.
        let exact = "é".repeat(40);
        assert_eq!(sample_of(&exact), exact);
        // One more char pushes it over; the result must still be valid
        // UTF-8 (the naive `&s[..80]` would split the last `é`).
        let over = "é".repeat(41);
        let got = sample_of(&over);
        assert!(got.ends_with('…'), "{got}");
        assert!(got.len() <= 80 + '…'.len_utf8());
    }

    #[test]
    fn a_lossy_rule_carries_the_name_r3_groups_by() {
        let p = Problem::lossy("pdf.strip_repeated_chrome", None, "Page 3 of 7");
        assert_eq!(p.reason, Reason::DeliberateLoss);
        assert_eq!(p.rule.as_deref(), Some("pdf.strip_repeated_chrome"));
    }

    /// An explanation longer than a value's sample stays whole, with
    /// the rule a lossy problem carries.
    #[test]
    fn a_lossy_explanation_is_kept_whole() {
        let why = "x".repeat(200);
        let p = Problem::lossy_explained("rule", None, &why);
        assert_eq!(p.sample, why);
        assert_eq!(p.reason, Reason::DeliberateLoss);
        assert_eq!(p.rule.as_deref(), Some("rule"));
    }

    /// The id is a function of what names the problem: the same
    /// problem twice is one row, two problems on one record are two,
    /// and the sample — which changes with the data's wording — is not
    /// part of it.
    #[test]
    fn ids_are_stable_and_distinct_per_instance() {
        let row = |field: &str, sample: &str| {
            ProblemRow::new(
                "slack",
                Stage::GridRow,
                Scope::Markdown("md-1"),
                Some("u-1"),
                Outcome::Nulled,
                Problem::field(field, Reason::CoercionFailed, sample),
                Some(3),
            )
        };
        let a = row("created_at", "yesterday");
        assert_eq!(a.problem_uuid, row("created_at", "tomorrow").problem_uuid);
        assert_ne!(a.problem_uuid, row("modified_at", "yesterday").problem_uuid);
        assert_eq!(a.severity, Severity::Warning);
        assert_eq!(a.scope_kind, ScopeKind::Markdown);
        assert_eq!(a.render_version, Some(3));
        let dropped = ProblemRow::new(
            "slack",
            Stage::Parse,
            Scope::Entity("e-1"),
            None,
            Outcome::Dropped,
            Problem::record(Reason::Undeserializable, "{"),
            None,
        );
        assert_eq!(dropped.severity, Severity::Error);
        assert_eq!(
            ProblemRow::new(
                "slack",
                Stage::Parse,
                Scope::Entity("e-1"),
                None,
                Outcome::Ok,
                Problem::record(Reason::Noted, "").severity(Severity::Warning),
                None,
            )
            .severity,
            Severity::Warning,
            "a writer that says a severity keeps it"
        );
    }

    /// A mute takes a row off the count of the step that found it, so a
    /// fetch row the render copied must still be the download's.
    #[test]
    fn each_row_is_counted_by_the_step_that_found_it() {
        assert_eq!(
            CountedBy::of(Stage::Fetch, ScopeKind::Entity, "emails:1"),
            CountedBy::Download
        );
        assert_eq!(
            CountedBy::of(Stage::Fetch, ScopeKind::Markdown, "md-1"),
            CountedBy::Download
        );
        assert_eq!(
            CountedBy::of(Stage::GridRow, ScopeKind::Markdown, "md-1"),
            CountedBy::Render
        );
        assert_eq!(
            CountedBy::of(Stage::Parse, ScopeKind::Entity, "e-1"),
            CountedBy::Render
        );
        assert_eq!(
            CountedBy::of(Stage::Render, ScopeKind::Entity, INDEX_OWN_SCOPE_KEY),
            CountedBy::Index
        );
        assert_eq!(
            CountedBy::of(Stage::TimeSeries, ScopeKind::Entity, "activities:1"),
            CountedBy::TimeSeries
        );
        assert_eq!(
            CountedBy::of(Stage::Occurrences, ScopeKind::Entity, "events:1"),
            CountedBy::Occurrences
        );
    }

    #[test]
    fn metric_labels_round_trip_through_the_run_stores_spelling() {
        let (k, v) = metric_label(Severity::Warning);
        assert_eq!(
            severity_in_metric_labels(&format!("{k}={v}")),
            Some(Severity::Warning)
        );
        assert_eq!(
            severity_in_metric_labels("table=x,severity=error"),
            Some(Severity::Error)
        );
        assert_eq!(severity_in_metric_labels("severity=loud"), None);
        assert_eq!(severity_in_metric_labels(""), None);
    }

    #[test]
    fn the_ddl_binds_every_enum_as_text() {
        let (_, ddl) = DDL[0];
        for col in [
            "stage VARCHAR(16) NOT NULL",
            "severity VARCHAR(16) NOT NULL",
            "render_version INT,",
        ] {
            assert!(ddl.contains(col), "{ddl}");
        }
        assert_eq!(COLUMNS[0].1.len(), 20);
    }
}
