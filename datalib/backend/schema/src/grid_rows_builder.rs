// Hand-written companion to the generated `GridRow` struct (see
// `generated/grid_rows.rs`). The generated file is a plain data struct
// with public fields; this module adds the one *blessed* way to build a
// row — a validating builder — so producers stop hand-writing 24-field
// literals (where a malformed value silently reaches the grid) and
// instead funnel through [`GridRow::builder`].

use datalib_time::validate_iso_offset;

use crate::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
use crate::providers::Provider;

/// Why a [`GridRowBuilder::build`] call was rejected.
#[derive(Debug)]
pub enum GridRowError {
    /// A required identity column was empty / whitespace-only.
    EmptyField { field: &'static str },
    /// `created_at` or `modified_at` was `Some` but not RFC 3339 with an
    /// explicit offset. The grid derives its sortable `_utc` twin from
    /// the stamp, so an unparseable value would sort wrong and render
    /// verbatim.
    InvalidStamp {
        field: &'static str,
        value: String,
        reason: String,
    },
}

impl std::fmt::Display for GridRowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GridRowError::EmptyField { field } => {
                write!(f, "grid_row field `{field}` must be non-empty")
            }
            GridRowError::InvalidStamp {
                field,
                value,
                reason,
            } => write!(
                f,
                "grid_row {field} {value:?} must be RFC 3339 with an explicit \
                 offset (e.g. 2026-06-16T00:00:00+00:00): {reason}"
            ),
        }
    }
}

impl std::error::Error for GridRowError {}

/// The `_utc` / `_offset` twins of a record stamp: the instant in UTC
/// at one width, so text order is time order, and the offset the
/// source wrote it in (`+05:30`, `-07:00`) so the UI can show the
/// wall-clock it was recorded at. Both come from one parse and are
/// always both present or both absent.
fn split(stamp: Option<&str>) -> Option<(String, String)> {
    stamp.and_then(datalib_time::split_record_stamp)
}

impl GridRow {
    pub fn derived_created_at_utc(&self) -> Option<String> {
        split(self.created_at.as_deref()).map(|(utc, _)| utc)
    }

    pub fn derived_created_offset(&self) -> Option<String> {
        split(self.created_at.as_deref()).map(|(_, offset)| offset)
    }

    pub fn derived_modified_at_utc(&self) -> Option<String> {
        split(self.modified_at.as_deref()).map(|(utc, _)| utc)
    }

    pub fn derived_modified_offset(&self) -> Option<String> {
        split(self.modified_at.as_deref()).map(|(_, offset)| offset)
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
    created_at: Option<String>,
    modified_at: Option<String>,
    is_document: bool,
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
    byte_size: Option<i64>,
    item_count: Option<i64>,
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
    req_setter!(kind);
    req_setter!(source_label);
    req_setter!(conversation_uuid);
    req_setter!(entire_chat);
    req_setter!(text);

    /// Set the required `provider` column. Typed, unlike its
    /// neighbours: the tag is a closed set, and it is on disk in the
    /// column, the frontmatter and the rendered-tree path.
    pub fn provider(mut self, v: Provider) -> Self {
        self.provider = v.as_str().to_string();
        self
    }

    opt_setter!(created_at);
    opt_setter!(modified_at);
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

    /// Mark this row as the one that *is* the rendered document. Exactly
    /// one row per document says so; the render store checks.
    pub fn is_document(mut self, v: bool) -> Self {
        self.is_document = v;
        self
    }

    pub fn message_index(mut self, v: impl Into<Option<i64>>) -> Self {
        self.message_index = v.into();
        self
    }

    pub fn byte_size(mut self, v: impl Into<Option<i64>>) -> Self {
        self.byte_size = v.into();
        self
    }

    pub fn item_count(mut self, v: impl Into<Option<i64>>) -> Self {
        self.item_count = v.into();
        self
    }

    /// Validate and finalize the row, recording what had to give. A
    /// stamp that will not parse is nulled and the row kept — a record
    /// with an identity is still a record, and the grid only loses its
    /// place in time order; a row with no identity is dropped and `None`
    /// comes back so the caller keeps going.
    pub fn build_or_record(
        mut self,
        source_id: &str,
        scope_key: &str,
        render_version: u32,
        problems: &mut Vec<ProblemRow>,
    ) -> Option<GridRow> {
        // Keep the identity before `build` consumes the builder, so a
        // rejected row can still be named. A row with no uuid has no
        // identity to key its problem on either; the id is then minted
        // from the scope and the field alone, which is stable across
        // runs, so the same bad record does not accumulate a new row
        // every run.
        let uuid = self.uuid.clone();
        let item_uuid = (!uuid.trim().is_empty()).then_some(uuid.as_str());
        let scope = Scope::Markdown(scope_key);
        for (field, slot) in [
            ("created_at", &mut self.created_at),
            ("modified_at", &mut self.modified_at),
        ] {
            let Some(ts) = slot.take() else { continue };
            if validate_iso_offset(&ts).is_ok() {
                *slot = Some(ts);
            } else {
                problems.push(ProblemRow::new(
                    source_id,
                    Stage::GridRow,
                    scope,
                    item_uuid,
                    Outcome::Nulled,
                    Problem::field(field, Reason::CoercionFailed, &ts),
                    Some(render_version),
                ));
            }
        }
        match self.build() {
            Ok(row) => Some(row),
            Err(e) => {
                let field = match &e {
                    GridRowError::EmptyField { field } => *field,
                    // Cleared above.
                    GridRowError::InvalidStamp { field, .. } => *field,
                };
                problems.push(ProblemRow::new(
                    source_id,
                    Stage::GridRow,
                    scope,
                    item_uuid,
                    Outcome::Dropped,
                    Problem::field(field, Reason::NoIdentity, ""),
                    Some(render_version),
                ));
                // Deliberately no `warn!` here. The render path's
                // diagnostics buffer is not installed (see the audit's
                // §1), so a log line from here reaches nobody — that is
                // the gap this sink exists to close, and logging into
                // the void beside it would only look like coverage.
                None
            }
        }
    }

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
        for (field, stamp) in [
            ("created_at", &self.created_at),
            ("modified_at", &self.modified_at),
        ] {
            if let Some(ts) = stamp {
                validate_iso_offset(ts).map_err(|e| GridRowError::InvalidStamp {
                    field,
                    value: ts.clone(),
                    reason: e.to_string(),
                })?;
            }
        }
        Ok(GridRow {
            uuid: self.uuid,
            provider: self.provider,
            kind: self.kind,
            source_label: self.source_label,
            created_at: self.created_at,
            modified_at: self.modified_at,
            is_document: self.is_document,
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
            byte_size: self.byte_size,
            item_count: self.item_count,
        })
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn ok_builder() -> GridRowBuilder {
        GridRow::builder()
            .uuid("u-1")
            .provider(Provider::Linkedin)
            .kind("Contact")
            .source_label("LinkedIn")
            .conversation_uuid("c-1")
            .entire_chat("/contact/u-1")
            .text("Jean-Luc Picard")
    }

    #[test]
    fn builds_minimal_row_with_none_created_at() {
        let row = ok_builder().build().expect("valid row");
        assert_eq!(row.uuid, "u-1");
        assert!(row.created_at.is_none());
        assert!(row.author.is_none());
    }

    #[test]
    fn accepts_offset_bearing_created_at() {
        let row = ok_builder()
            .created_at(Some("2026-06-16T00:00:00+00:00".to_string()))
            .build()
            .expect("offset-bearing ts is valid");
        assert_eq!(row.created_at.as_deref(), Some("2026-06-16T00:00:00+00:00"));
    }

    #[test]
    fn rejects_bare_date_created_at() {
        // The LinkedIn "Connected On" bug: a bare "DD Mon YYYY" date has
        // no time and no offset, so it can't be a valid created_at.
        let err = ok_builder()
            .created_at(Some("16 Jun 2026".to_string()))
            .build()
            .expect_err("bare date must be rejected");
        assert!(matches!(err, GridRowError::InvalidStamp { .. }), "{err}");
    }

    #[test]
    fn rejects_naive_datetime_without_offset() {
        let err = ok_builder()
            .created_at(Some("2026-06-16T00:00:00".to_string()))
            .build()
            .expect_err("offset is required");
        assert!(matches!(err, GridRowError::InvalidStamp { .. }), "{err}");
    }

    #[test]
    fn rejects_empty_required_field() {
        let err = ok_builder().uuid("").build().expect_err("empty uuid");
        assert!(matches!(err, GridRowError::EmptyField { field: "uuid" }));
    }

    /// R1 in `data_architecture_parse_and_render.md`: a field that fails
    /// its coercion is nulled and the record kept. A merge request whose
    /// `created_at` was garbled still has an identity, a title and a
    /// body; dropping the row lost the whole document over one
    /// unsortable field.
    #[test]
    fn a_bad_created_at_is_nulled_and_the_row_kept() {
        let mut problems = Vec::new();
        let row = ok_builder()
            .created_at(Some("16 Jun 2026".to_string()))
            .build_or_record("src", "doc-1", 3, &mut problems)
            .expect("the row survives");
        assert!(row.created_at.is_none());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].item_uuid.as_deref(), Some("u-1"));
        assert_eq!(problems[0].scope_key, "doc-1");
        assert_eq!(problems[0].outcome, Outcome::Nulled);
        assert_eq!(problems[0].field.as_deref(), Some("created_at"));
        assert_eq!(problems[0].sample, "16 Jun 2026");
    }

    /// Two bad stamps on one record are two rows with two ids — the
    /// store's primary key is the problem, not the record. Under the
    /// old item-keyed table this was a constraint failure on the
    /// second insert.
    #[test]
    fn two_bad_stamps_are_two_rows() {
        let mut problems = Vec::new();
        let row = ok_builder()
            .created_at(Some("16 Jun 2026".to_string()))
            .modified_at(Some("yesterday".to_string()))
            .build_or_record("src", "doc-1", 3, &mut problems)
            .expect("the row survives");
        assert!(row.created_at.is_none() && row.modified_at.is_none());
        assert_eq!(problems.len(), 2);
        assert_ne!(problems[0].problem_uuid, problems[1].problem_uuid);
        let mut again = Vec::new();
        ok_builder()
            .created_at(Some("16 Jun 2026".to_string()))
            .modified_at(Some("yesterday".to_string()))
            .build_or_record("src", "doc-1", 3, &mut again);
        assert_eq!(
            problems.iter().map(|p| &p.problem_uuid).collect::<Vec<_>>(),
            again.iter().map(|p| &p.problem_uuid).collect::<Vec<_>>(),
            "the same record on the next run mints the same ids"
        );
    }

    /// `modified_at` is held to the same form as `created_at`, and the
    /// problem row names which of the two gave.
    #[test]
    fn a_bad_modified_at_is_nulled_by_name() {
        let mut problems = Vec::new();
        let row = ok_builder()
            .created_at(Some("2026-06-16T00:00:00+00:00".to_string()))
            .modified_at(Some("yesterday".to_string()))
            .build_or_record("src", "doc-1", 3, &mut problems)
            .expect("the row survives");
        assert_eq!(row.created_at.as_deref(), Some("2026-06-16T00:00:00+00:00"));
        assert!(row.modified_at.is_none());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].field.as_deref(), Some("modified_at"));
        let err = ok_builder()
            .modified_at(Some("yesterday".to_string()))
            .build()
            .expect_err("rejected outright by build");
        assert!(
            matches!(
                err,
                GridRowError::InvalidStamp {
                    field: "modified_at",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_row_is_not_a_document_unless_it_says_so() {
        assert!(!ok_builder().build().unwrap().is_document);
        assert!(ok_builder().is_document(true).build().unwrap().is_document);
    }

    #[test]
    fn a_row_without_identity_is_dropped() {
        let mut problems = Vec::new();
        let row = ok_builder()
            .uuid("")
            .build_or_record("src", "doc-1", 3, &mut problems);
        assert!(row.is_none());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].item_uuid.is_none());
        assert_eq!(problems[0].outcome, Outcome::Dropped);
        assert_eq!(problems[0].reason, Reason::NoIdentity);
        assert_eq!(problems[0].field.as_deref(), Some("uuid"));
    }
}
