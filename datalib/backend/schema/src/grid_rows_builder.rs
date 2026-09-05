// Hand-written companion to the generated `GridRow` struct (see
// `generated/grid_rows.rs`). The generated file is a plain data struct
// with public fields; this module adds the one *blessed* way to build a
// row — a validating builder — so producers stop hand-writing 24-field
// literals (where a malformed value silently reaches the grid) and
// instead funnel through [`GridRow::builder`].
//
// Validation deliberately lives here, at construction time, rather than
// at DB-insert time: a bad `when_ts` used to slip all the way to
// `load::insert_grid_row`, where `split_when_ts` quietly returned `None`
// and left the raw upstream string (e.g. LinkedIn's `"16 Jun 2026"`) in
// the displayed column. Catching it in `build()` turns that silent
// display bug into a loud error a provider's own tests trip over.

use datalib_time::validate_iso_offset;

use crate::render_problems::{sample_of, Outcome, Problem, Reason, RenderProblemRow};

/// Short content hash for a record with no usable identity.
///
/// Not `blake3` the crate — `datalib_schema` has no hashing dependency
/// and does not need one for a 16-char surrogate whose only job is to be
/// stable for the same bad record across runs.
fn blake3_hex(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), s.len())
}

/// Why a [`GridRowBuilder::build`] call was rejected.
#[derive(Debug)]
pub enum GridRowError {
    /// A required identity column was empty / whitespace-only.
    EmptyField { field: &'static str },
    /// `when_ts` was `Some` but not RFC 3339 with an explicit offset.
    /// The grid derives its sortable `when_ts_utc` column from this, so
    /// an unparseable value would sort wrong and render verbatim.
    InvalidWhenTs { value: String, reason: String },
}

impl std::fmt::Display for GridRowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GridRowError::EmptyField { field } => {
                write!(f, "grid_row field `{field}` must be non-empty")
            }
            GridRowError::InvalidWhenTs { value, reason } => write!(
                f,
                "grid_row when_ts {value:?} must be RFC 3339 with an explicit \
                 offset (e.g. 2026-06-16T00:00:00+00:00): {reason}"
            ),
        }
    }
}

impl std::error::Error for GridRowError {}

impl GridRow {
    /// `when_ts_utc` — the same instant normalized to UTC, fixed
    /// microsecond width, `Z` suffix.
    ///
    /// Derived rather than stored on the struct because producers never
    /// set it: a single zone and a single width make lexical order match
    /// true chronological order, which a column of mixed local-offset
    /// `when_ts` strings does not — `2026-01-01T09:00:00+00:00` sorts
    /// before `2026-01-01T10:00:00-08:00` as text and is nine hours
    /// earlier in fact. This is the column the grid sorts and
    /// `before:`/`after:`-filters on.
    ///
    /// An absent or unparseable `when_ts` leaves it NULL — never a
    /// fabricated value, per
    /// `docs/dev/data_architecture_parse_and_render.md` §6.
    ///
    /// Lives here, next to the column declaration that documents it,
    /// rather than in the index writer that used to compute it inline:
    /// the derivation is part of the schema, and the `PortableTable`
    /// derive calls this by name.
    pub fn derived_when_ts_utc(&self) -> Option<String> {
        self.when_ts
            .as_deref()
            .and_then(datalib_time::split_when_ts)
            .map(|(utc, _offset)| utc)
    }

    /// `when_offset` — the original UTC offset (`+05:30`, `-07:00`),
    /// preserved so the UI can re-render the instant in the wall-clock
    /// zone it was recorded in. NULL whenever
    /// [`Self::derived_when_ts_utc`] is NULL; the two are derived from
    /// one parse and are always both present or both absent.
    pub fn derived_when_offset(&self) -> Option<String> {
        self.when_ts
            .as_deref()
            .and_then(datalib_time::split_when_ts)
            .map(|(_utc, offset)| offset)
    }
}

impl GridRow {
    /// Start building a [`GridRow`]. Set only the columns you need — the
    /// ~17 optional ones default to `None` — then call
    /// [`GridRowBuilder::build`]. This is the supported construction path;
    /// it validates the row so malformed data fails at the producer
    /// instead of silently corrupting the grid.
    pub fn builder() -> GridRowBuilder {
        GridRowBuilder::default()
    }
}

/// Defaulted accumulator for [`GridRow`]. See [`GridRow::builder`].
#[derive(Default, Clone)]
pub struct GridRowBuilder {
    uuid: String,
    provider: String,
    kind: String,
    source_label: String,
    when_ts: Option<String>,
    author: Option<String>,
    account: Option<String>,
    project: Option<String>,
    org_uuid: Option<String>,
    org_name: Option<String>,
    channel: Option<String>,
    conversation_name: Option<String>,
    conversation_uuid: String,
    message_index: Option<i64>,
    entire_chat: String,
    text: String,
    slack_link: Option<String>,
    qmd_path: Option<String>,
    source_url: Option<String>,
    git_sha: Option<String>,
    upstream_id: Option<String>,
    upstream_entity_kind: Option<String>,
    upstream_scope: Option<String>,
    notion_page_uuid: Option<String>,
    notion_block_uuid: Option<String>,
    markdown_uuid: Option<String>,
}

/// Generate a required-field setter (`impl Into<String>`).
macro_rules! req_setter {
    ($name:ident) => {
        #[doc = concat!("Set the required `", stringify!($name), "` column.")]
        pub fn $name(mut self, v: impl Into<String>) -> Self {
            self.$name = v.into();
            self
        }
    };
}

/// Generate an optional `String` setter. Accepts `Some(x)`, a bare
/// `String`, or a typed `Option<String>`; omit the call entirely to
/// leave the column `None`.
macro_rules! opt_setter {
    ($name:ident) => {
        #[doc = concat!("Set the optional `", stringify!($name), "` column.")]
        pub fn $name(mut self, v: impl Into<Option<String>>) -> Self {
            self.$name = v.into();
            self
        }
    };
}

impl GridRowBuilder {
    req_setter!(uuid);
    req_setter!(provider);
    req_setter!(kind);
    req_setter!(source_label);
    req_setter!(conversation_uuid);
    req_setter!(entire_chat);
    req_setter!(text);

    opt_setter!(when_ts);
    opt_setter!(author);
    opt_setter!(account);
    opt_setter!(project);
    opt_setter!(org_uuid);
    opt_setter!(org_name);
    opt_setter!(channel);
    opt_setter!(conversation_name);
    opt_setter!(slack_link);
    opt_setter!(qmd_path);
    opt_setter!(source_url);
    opt_setter!(git_sha);
    opt_setter!(upstream_id);
    opt_setter!(upstream_entity_kind);
    opt_setter!(upstream_scope);
    opt_setter!(notion_page_uuid);
    opt_setter!(notion_block_uuid);
    opt_setter!(markdown_uuid);

    /// Set the optional `message_index` column (within-conversation
    /// ordinal). Accepts `Some(i)` or a bare `i64`.
    pub fn message_index(mut self, v: impl Into<Option<i64>>) -> Self {
        self.message_index = v.into();
        self
    }

    /// Validate and finalize the row, or report why it could not be
    /// built and return `None` so the caller drops it and keeps going.
    ///
    /// This is R2's second category made expressible. `build` offers one
    /// failure mode, and every callsite in the tree propagated it with
    /// `?` — so a single row with an unparseable `when_ts` failed the
    /// whole source's render, which the DAG then classified as `data`
    /// and used to poison every step below it, `grid_index` included.
    /// One bad record out of forty thousand stopped the grid updating
    /// for every provider.
    ///
    /// A dropped row is pushed onto `problems` as a `Dropped` /
    /// `NoIdentity`-or-`CoercionFailed` entry, so the record of what was
    /// lost lands beside the rows that survived — never a count without
    /// a reason, never a reason without a sample.
    ///
    /// The caller supplies `scope_key` (the `markdown_uuid` this row
    /// belongs to) and `source_name`, because a `GridRow` on its own
    /// does not know which document it was headed for.
    pub fn build_or_record(
        self,
        source_name: &str,
        scope_key: &str,
        render_version: u32,
        problems: &mut Vec<RenderProblemRow>,
    ) -> Option<GridRow> {
        // Keep the identity before `build` consumes the builder, so a
        // rejected row can still be named.
        let uuid = self.uuid.clone();
        match self.build() {
            Ok(row) => Some(row),
            Err(e) => {
                let (field, reason, sample) = match &e {
                    GridRowError::EmptyField { field } => (
                        Some((*field).to_string()),
                        Reason::NoIdentity,
                        String::new(),
                    ),
                    GridRowError::InvalidWhenTs { value, .. } => (
                        Some("when_ts".to_string()),
                        Reason::CoercionFailed,
                        value.clone(),
                    ),
                };
                // A row with no uuid has no identity to key the problem
                // on either; give it the content-derived surrogate so
                // the same bad record does not accumulate a new row
                // every run.
                let key = if uuid.trim().is_empty() {
                    format!(
                        "noid:{}",
                        &blake3_hex(&format!("{source_name}\x1f{scope_key}\x1f{e}"))[..16]
                    )
                } else {
                    uuid
                };
                let problem = Problem {
                    field,
                    path: None,
                    reason,
                    rule: None,
                    sample: sample_of(&sample),
                };
                problems.push(RenderProblemRow {
                    uuid: key,
                    scope_key: scope_key.to_string(),
                    scope_kind: "markdown".to_string(),
                    source_name: source_name.to_string(),
                    stage: "grid_row".to_string(),
                    outcome: Outcome::Dropped.as_str().to_string(),
                    problems: serde_json::to_string(&vec![problem]).unwrap_or_else(|_| "[]".into()),
                    // Left for the store to stamp; it is the only
                    // layer that can see whether this uuid already had
                    // a row, and so the only one that can tell "first
                    // seen" from "seen again". See the field docs.
                    first_seen_at: String::new(),
                    last_seen_at: String::new(),
                    render_version: render_version as i64,
                });
                // Deliberately no `warn!` here. The render path's
                // diagnostics buffer is not installed (see the audit's
                // §1), so a log line from here reaches nobody — that is
                // the gap this sink exists to close, and logging into
                // the void beside it would only look like coverage.
                None
            }
        }
    }

    /// Validate and finalize the row.
    ///
    /// Rejects an empty `uuid` / `provider` / `kind` / `source_label`,
    /// and a `when_ts` that isn't RFC 3339 with an explicit offset (the
    /// grid's sortable `when_ts_utc` column is derived from it). `None`
    /// `when_ts` is fine — it means "no source-side timestamp", which we
    /// never fabricate.
    pub fn build(self) -> Result<GridRow, GridRowError> {
        for (field, val) in [
            ("uuid", &self.uuid),
            ("provider", &self.provider),
            ("kind", &self.kind),
            ("source_label", &self.source_label),
        ] {
            if val.trim().is_empty() {
                return Err(GridRowError::EmptyField { field });
            }
        }
        if let Some(ts) = &self.when_ts {
            validate_iso_offset(ts).map_err(|e| GridRowError::InvalidWhenTs {
                value: ts.clone(),
                reason: e.to_string(),
            })?;
        }
        Ok(GridRow {
            uuid: self.uuid,
            provider: self.provider,
            kind: self.kind,
            source_label: self.source_label,
            when_ts: self.when_ts,
            author: self.author,
            account: self.account,
            project: self.project,
            org_uuid: self.org_uuid,
            org_name: self.org_name,
            channel: self.channel,
            conversation_name: self.conversation_name,
            conversation_uuid: self.conversation_uuid,
            message_index: self.message_index,
            entire_chat: self.entire_chat,
            text: self.text,
            slack_link: self.slack_link,
            qmd_path: self.qmd_path,
            source_url: self.source_url,
            git_sha: self.git_sha,
            upstream_id: self.upstream_id,
            upstream_entity_kind: self.upstream_entity_kind,
            upstream_scope: self.upstream_scope,
            notion_page_uuid: self.notion_page_uuid,
            notion_block_uuid: self.notion_block_uuid,
            markdown_uuid: self.markdown_uuid,
        })
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn ok_builder() -> GridRowBuilder {
        GridRow::builder()
            .uuid("u-1")
            .provider("linkedin")
            .kind("Contact")
            .source_label("LinkedIn")
            .conversation_uuid("c-1")
            .entire_chat("/contact/u-1")
            .text("Jean-Luc Picard")
    }

    #[test]
    fn builds_minimal_row_with_none_when_ts() {
        let row = ok_builder().build().expect("valid row");
        assert_eq!(row.uuid, "u-1");
        assert!(row.when_ts.is_none());
        assert!(row.author.is_none());
    }

    #[test]
    fn accepts_offset_bearing_when_ts() {
        let row = ok_builder()
            .when_ts(Some("2026-06-16T00:00:00+00:00".to_string()))
            .build()
            .expect("offset-bearing ts is valid");
        assert_eq!(row.when_ts.as_deref(), Some("2026-06-16T00:00:00+00:00"));
    }

    #[test]
    fn rejects_bare_date_when_ts() {
        // The LinkedIn "Connected On" bug: a bare "DD Mon YYYY" date has
        // no time and no offset, so it can't be a valid when_ts.
        let err = ok_builder()
            .when_ts(Some("16 Jun 2026".to_string()))
            .build()
            .expect_err("bare date must be rejected");
        assert!(matches!(err, GridRowError::InvalidWhenTs { .. }), "{err}");
    }

    #[test]
    fn rejects_naive_datetime_without_offset() {
        let err = ok_builder()
            .when_ts(Some("2026-06-16T00:00:00".to_string()))
            .build()
            .expect_err("offset is required");
        assert!(matches!(err, GridRowError::InvalidWhenTs { .. }), "{err}");
    }

    #[test]
    fn rejects_empty_required_field() {
        let err = ok_builder().uuid("").build().expect_err("empty uuid");
        assert!(matches!(err, GridRowError::EmptyField { field: "uuid" }));
    }
}
