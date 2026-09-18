//! The `problems` table: one row per thing a step could not fully do to
//! one record, kept in the store the step owns and copied downstream
//! with the data until it reaches the unified index and the screen.
//! Every closed vocabulary here is an enum: the row binds them as text
//! and a reader parses them back, never comparing the column to a
//! literal. The design — ids, severity, the copy rule, what reads the
//! table — is `docs/dev/plans/problem_visibility.md`.

use anyhow::{Context, Result};
use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};
use sqlx::Row;

macro_rules! closed_vocabulary {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            Serialize,
            Deserialize,
            strum::EnumString,
            strum::IntoStaticStr,
            strum::VariantArray,
        )]
        #[serde(rename_all = "snake_case")]
        #[strum(serialize_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                self.into()
            }

            /// `None` for a spelling this build does not know.
            pub fn parse(s: &str) -> Option<Self> {
                s.parse().ok()
            }
        }
    };
}

closed_vocabulary! {
    /// How loudly the screen says it. The count on the Manage row is
    /// errors and warnings; `info` is a grid filter and never a badge.
    Severity {
        Error,
        Warning,
        Info,
    }
}

closed_vocabulary! {
    /// Which stage noticed the problem. The fix is in a different place
    /// for each.
    Stage {
        /// Fetching the record from upstream.
        Fetch,
        /// Deserializing the stored payload.
        Parse,
        /// Projecting it to markdown.
        Render,
        /// Building the `grid_rows` row.
        GridRow,
    }
}

closed_vocabulary! {
    /// What happened to the record this row is about — data-loss
    /// semantics, kept apart from [`Severity`], which is presentation.
    Outcome {
        /// The record did not reach the index at all.
        Dropped,
        /// It did, with at least one field discarded.
        Nulled,
        /// It did, intact. A finding worth publishing, claiming nothing
        /// was lost — R6 in miniature.
        Ok,
    }
}

closed_vocabulary! {
    /// Why one field or record is being reported.
    Reason {
        /// The stored payload would not deserialize. → drop the record.
        Undeserializable,
        /// No usable identity, so nothing could be keyed on. → drop.
        NoIdentity,
        /// A field failed its declared coercion. → null that field, keep
        /// the record.
        CoercionFailed,
        /// A value whose type the contract does not cover. → null that
        /// field; never pass it through untyped.
        UncoveredType,
        /// A deliberate lossy rule fired (truncation, chrome-stripping).
        /// These are the rows R3's judgment-call table is generated
        /// from, which is why `Problem::rule` exists.
        DeliberateLoss,
        /// The projection itself failed on this record — a converter
        /// error, a renderer that gave up on the document. → drop it
        /// this run; a page from an earlier run may still be on disk.
        RenderFailed,
        /// Nothing was lost; this is a finding worth publishing.
        Noted,
    }
}

closed_vocabulary! {
    /// Which of the two things [`ProblemRow::scope_key`] holds — the
    /// sweep key's type, and so which `DELETE … WHERE scope_kind = ?`
    /// clears this row when its owner is reprocessed.
    ScopeKind {
        /// `scope_key` is the `markdown_uuid` of the document the record
        /// belongs to. The usual case, and the link to the document.
        Markdown,
        /// `scope_key` is the raw-store entity id, because the failure
        /// happened before we knew which document the record was for.
        Entity,
    }
}

/// The metric series a step reports its whole-store problem counts on,
/// one sample per severity under the [`METRIC_LABEL`] label, every run
/// and zero included: the Manage screen reads a missing series as
/// "never counted", not as clean.
pub const METRIC: &str = "problems";
pub const METRIC_LABEL: &str = "severity";

impl Severity {
    /// The `severity=<word>` label a [`METRIC`] sample carries.
    pub fn metric_label(self) -> (&'static str, &'static str) {
        (METRIC_LABEL, self.as_str())
    }

    /// The severity a run-store label string names, `severity=error`
    /// as the store canonicalizes it.
    pub fn from_metric_labels(labels: &str) -> Option<Severity> {
        labels
            .split(',')
            .find_map(|kv| {
                kv.strip_prefix(METRIC_LABEL)
                    .and_then(|v| v.strip_prefix('='))
            })
            .and_then(Severity::parse)
    }

    /// The severity a writer gets when it says nothing more: a record
    /// that was lost is an error, one that was degraded a warning, one
    /// that survived intact a finding.
    pub fn default_for(outcome: Outcome) -> Severity {
        match outcome {
            Outcome::Dropped => Severity::Error,
            Outcome::Nulled => Severity::Warning,
            Outcome::Ok => Severity::Info,
        }
    }
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
    /// The R3 judgment-call rule that fired, when this was a deliberate
    /// lossy rule rather than a defect. Must be stable across runs —
    /// name it for the rule, not for the value it happened to see.
    pub rule: Option<String>,
    /// First 80 characters of the offending value — see [`sample_of`].
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

    /// A deliberate lossy rule fired. `rule` is what R3's generated
    /// table groups by.
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
#[portable_table(table = "problems", primary_key = "problem_uuid")]
pub struct ProblemRow {
    /// Minted by `datalib_id::problem_id` from the columns that name
    /// the problem — never from the sample or the stamps — so a re-run
    /// of the same code over the same record mints the same id.
    #[col(sql = "VARCHAR(36)")]
    pub problem_uuid: String,
    /// The id of the source that produced this, matching
    /// `markdowns.source_id`.
    #[col(sql = "VARCHAR(64)")]
    pub source_id: String,
    #[col(sql = "VARCHAR(16)", enum)]
    pub stage: Stage,
    #[col(sql = "VARCHAR(16)", enum)]
    pub severity: Severity,
    #[col(sql = "VARCHAR(16)", enum)]
    pub outcome: Outcome,
    #[col(sql = "VARCHAR(32)", enum)]
    pub reason: Reason,
    #[col(sql = "VARCHAR(16)", enum)]
    pub scope_kind: ScopeKind,
    /// The sweep key: the `markdown_uuid` of the document the record
    /// belongs to, or — when the failure happened before we knew that —
    /// the raw-store entity id.
    #[col(sql = "VARCHAR(96)")]
    pub scope_key: String,
    /// The `grid_rows.uuid` the record has, or would have had; `None`
    /// for a problem about the whole document or entity. With a
    /// markdown scope this is the section the document view can scroll
    /// to.
    #[col(sql = "VARCHAR(96)")]
    pub item_uuid: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub field: Option<String>,
    #[col(sql = "VARCHAR(255)")]
    pub path: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub rule: Option<String>,
    #[col(sql = "TEXT")]
    pub sample: String,
    /// When this problem was first recorded, in UTC. Stamped by the
    /// store that first holds it, not the writer, and carried through
    /// every copy downstream.
    #[col(sql = "VARCHAR(40)")]
    pub first_seen_at_utc: String,
    /// When it was last re-recorded. Equal to `first_seen_at_utc` on a
    /// problem seen once. Also stamped by the store.
    #[col(sql = "VARCHAR(40)")]
    pub last_seen_at_utc: String,
    /// The offset the store's clock was in at `last_seen_at_utc`.
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// The `RENDER_VERSION` of the renderer that recorded it, so a row
    /// left by an older renderer is identifiable. `None` off a fetch.
    #[col(sql = "INT")]
    pub render_version: Option<i64>,
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
            last_seen_at_utc: String::new(),
            tz_offset: None,
            render_version: render_version.map(i64::from),
        }
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
            last_seen_at_utc: r.try_get("last_seen_at_utc")?,
            tz_offset: r.try_get("tz_offset")?,
            render_version: r.try_get("render_version")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

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

    /// Every vocabulary is written to SQL through strum's `as_str` and
    /// to JSON through serde. Two spellings of one value would make a
    /// sweep miss the rows it is supposed to clear.
    #[test]
    fn as_str_matches_the_serde_spelling_and_parses_back() {
        fn check<T>(variants: &[T])
        where
            T: Copy
                + std::fmt::Debug
                + PartialEq
                + Serialize
                + Into<&'static str>
                + std::str::FromStr,
        {
            for &v in variants {
                let s: &'static str = v.into();
                assert_eq!(
                    serde_json::to_string(&v).unwrap(),
                    format!("\"{s}\""),
                    "{v:?}"
                );
                assert_eq!(s.parse::<T>().ok(), Some(v), "{v:?}");
            }
        }
        check(Severity::VARIANTS);
        check(Stage::VARIANTS);
        check(Outcome::VARIANTS);
        check(Reason::VARIANTS);
        check(ScopeKind::VARIANTS);
    }

    #[test]
    fn a_lossy_rule_carries_the_name_r3_groups_by() {
        let p = Problem::lossy("pdf.strip_repeated_chrome", None, "Page 3 of 7");
        assert_eq!(p.reason, Reason::DeliberateLoss);
        assert_eq!(p.rule.as_deref(), Some("pdf.strip_repeated_chrome"));
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

    #[test]
    fn metric_labels_round_trip_through_the_run_stores_spelling() {
        let (k, v) = Severity::Warning.metric_label();
        assert_eq!(
            Severity::from_metric_labels(&format!("{k}={v}")),
            Some(Severity::Warning)
        );
        assert_eq!(
            Severity::from_metric_labels("table=x,severity=error"),
            Some(Severity::Error)
        );
        assert_eq!(Severity::from_metric_labels("severity=loud"), None);
        assert_eq!(Severity::from_metric_labels(""), None);
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
        assert_eq!(COLUMNS[0].1.len(), 17);
    }
}
