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

use frankweiler_time::validate_iso_offset;

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
    external_id: Option<String>,
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
    opt_setter!(external_id);
    opt_setter!(notion_page_uuid);
    opt_setter!(notion_block_uuid);
    opt_setter!(markdown_uuid);

    /// Set the optional `message_index` column (within-conversation
    /// ordinal). Accepts `Some(i)` or a bare `i64`.
    pub fn message_index(mut self, v: impl Into<Option<i64>>) -> Self {
        self.message_index = v.into();
        self
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
            external_id: self.external_id,
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
