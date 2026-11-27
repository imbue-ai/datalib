//! The `problem` and `covered` lines (`step_protocol.md` §"Problems: what
//! you could not do"), and the closed sets a problem is described in. A
//! problem is one thing a step could not fully do: a record it could not
//! fetch, a file it could not read.

use serde::{Deserialize, Serialize};

crate::closed_set! {
    /// How loudly the app shows a problem. The count on the Manage row is
    /// errors and warnings; `info` is a grid filter and never a count.
    pub enum Severity {
        Error = "error",
        Warning = "warning",
        Info = "info",
    }
}

impl Severity {
    /// The severity of a problem that names none: a record that was lost
    /// is an error, one that was degraded a warning, one that survived
    /// intact a finding.
    pub fn default_for(outcome: Outcome) -> Severity {
        match outcome {
            Outcome::Dropped => Severity::Error,
            Outcome::Nulled => Severity::Warning,
            Outcome::Ok => Severity::Info,
        }
    }
}

crate::closed_set! {
    /// Which stage noticed the problem. The fix is in a different place
    /// for each.
    pub enum Stage {
        /// Fetching the record from upstream.
        Fetch = "fetch",
        /// Deserializing the stored payload.
        Parse = "parse",
        /// Projecting it to markdown.
        Render = "render",
        /// Building the record's row in the grid.
        GridRow = "grid_row",
        /// Reading a source's time series.
        TimeSeries = "time_series",
        /// Listing a calendar's occurrences.
        Occurrences = "occurrences",
    }
}

crate::closed_set! {
    /// What happened to the record the problem is about. Kept apart from
    /// [`Severity`], which is how it is shown.
    pub enum Outcome {
        /// The record did not reach the index at all.
        Dropped = "dropped",
        /// It did, with at least one field discarded.
        Nulled = "nulled",
        /// It did, intact: a finding that claims nothing was lost.
        Ok = "ok",
    }
}

crate::closed_set! {
    /// Why one field or record is reported.
    pub enum Reason {
        /// The stored payload would not deserialize; the record is dropped.
        Undeserializable = "undeserializable",
        /// No usable identity, so nothing could be keyed on; dropped.
        NoIdentity = "no_identity",
        /// A field failed its declared coercion; that field is nulled and
        /// the record kept.
        CoercionFailed = "coercion_failed",
        /// A value of a type the contract does not cover; that field is
        /// nulled, never passed through untyped.
        UncoveredType = "uncovered_type",
        /// A piece of a document the renderer does not know how to draw,
        /// shown in place as the JSON upstream sent.
        UnsupportedJson = "unsupported_json",
        /// A deliberate lossy rule fired (truncation, stripping a page's
        /// repeated header); the problem's `rule` names it.
        DeliberateLoss = "deliberate_loss",
        /// The projection failed on this record, and it is dropped this
        /// run; an earlier run's document may still be there.
        RenderFailed = "render_failed",
        /// The download could not fetch this record: it is missing, or
        /// stale where an earlier fetch stored it.
        FetchFailed = "fetch_failed",
        /// The download once stored this attachment's bytes, and they are
        /// gone; it renders without them.
        BlobMissing = "blob_missing",
        /// A limit in the config said not to fetch this record. Nothing
        /// failed, but the record is not in the mirror.
        OverSizeLimit = "over_size_limit",
        /// A configured entry (a label, a channel) that upstream does not
        /// have; reported every run until the config is corrected.
        NotFound = "not_found",
        /// A configured entry that exists and this credential cannot read.
        Forbidden = "forbidden",
        /// A setting the run did not use because another one the config
        /// gives takes its place.
        Ignored = "ignored",
        /// A unit a render looked at produced no document, though its rows
        /// are still upstream; an earlier document is kept.
        NoDocument = "no_document",
        /// A configured entry upstream has sent nothing new for a while,
        /// such as a sensor out of battery.
        Silent = "silent",
        /// Nothing was lost; a finding worth publishing.
        Noted = "noted",
    }
}

crate::closed_set! {
    /// What a problem's `scope_key` holds.
    pub enum ScopeKind {
        /// The id of the document the record belongs to.
        Markdown = "markdown",
        /// The step's own key for the record, file or part of its run:
        /// `<table>:<id>`, `file:<path>`, `listing:<name>` or
        /// `phase:<name>`. What a [`Covered`] line matches.
        Entity = "entity",
    }
}

/// A `problem` line: one thing the step could not fully do. The runner
/// fills the rest of the row (its id, the step's group, the times).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub stage: Stage,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub outcome: Outcome,
    pub reason: Reason,
    /// [`Severity::default_for`] the outcome when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// What happened, in a few words: `HTTP 404`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sample: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_uuid: Option<String>,
    /// The field the problem is about; `None` for the whole record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For [`Reason::DeliberateLoss`], the rule that fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// What the problem is about upstream: the kind of thing, its id and
    /// the account, as the upstream service names them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_entity_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_account: Option<String>,
}

impl Problem {
    /// A problem with only the fields every one has.
    pub fn new(
        stage: Stage,
        scope_kind: ScopeKind,
        scope_key: impl Into<String>,
        outcome: Outcome,
        reason: Reason,
    ) -> Problem {
        Problem {
            stage,
            scope_kind,
            scope_key: scope_key.into(),
            outcome,
            reason,
            severity: None,
            sample: String::new(),
            item_uuid: None,
            field: None,
            path: None,
            rule: None,
            upstream_entity_kind: None,
            upstream_id: None,
            upstream_account: None,
        }
    }
}

/// A `covered` line: what this run checked again, so that a problem it
/// covered and did not find again is cleared. A `records` or `files` line
/// with neither `all_but` nor `only` vouches for every one of them, and
/// counts only when the run succeeds; with one, it counts even when the
/// run fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Covered {
    /// Everything; counts only when the run succeeds.
    All,
    /// Every `listing:` and `phase:` scope; counts only when the run
    /// succeeds.
    Listings,
    /// The `<table>:<id>` scopes of one table: every one but `all_but`,
    /// or exactly `only`.
    Records {
        table: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        all_but: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<Vec<String>>,
    },
    /// The `file:<path>` scopes under a folder, but `all_but`.
    Files {
        under: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        all_but: Option<Vec<String>>,
    },
}
