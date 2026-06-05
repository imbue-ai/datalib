//! `MirrorRepo` is the single seam between the HTTP layer and the
//! underlying SQL store.
//!
//! Sole implementation today: [`crate::dolt_repo::DoltRepo`] —
//! `sqlx::SqlitePool` against a doltlite file on disk.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db::ChatMeta;
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::search::SearchRow;
use frankweiler_schema::edges::EdgeRow;
use frankweiler_schema::feedback::FeedbackRow;
use frankweiler_schema::sync_jobs::SyncJobRow;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend does not support write operations")]
    ReadOnly,
    #[error("internal: {0}")]
    Internal(String),
}

/// The single point that all backend SQL flows through.
#[async_trait]
pub trait MirrorRepo: Send + Sync {
    /// Run a grid-search query and return rows for the UI.
    async fn search(&self, query: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError>;

    /// Fetch the per-markdown header data (title, account, channel, …)
    /// for the chat preview pane. Returns `Ok(None)` when no row
    /// matches. `markdown_uuid` is the canonical addressing primitive
    /// — the same UUID `/api/chat/{markdown_uuid}` takes.
    async fn chat_meta(&self, markdown_uuid: &str) -> Result<Option<ChatMeta>, RepoError>;

    /// Resolve the on-disk QMD path for one rendered markdown, keyed
    /// by `markdowns.markdown_uuid`. The returned path is absolute
    /// (already joined with the data root). This is the only file
    /// lookup left after the document_uuid → markdown_uuid cleanup:
    /// one UUID per rendered file, no enumeration, no fallbacks.
    async fn qmd_path_for_markdown(
        &self,
        markdown_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError>;

    /// Fetch every row's `(uuid, kind, qmd_path, provider)` tuple. Used to
    /// build a `GridIndex` so qmd-routed search can map hits → grid rows.
    /// Returning an empty list is acceptable for an empty / missing store.
    async fn grid_row_refs(&self) -> Result<Vec<GridRowRef>, RepoError>;

    /// Same shape as [`search`](Self::search), but with a caller-supplied
    /// ranked uuid list (output of `GridIndex::rows_for_hits`). The free-text
    /// portion of `q` is ignored — qmd has already done that work. Structured
    /// filters and date ranges still apply. Output preserves the input order.
    async fn search_by_uuids(
        &self,
        q: &ParsedQuery,
        uuids: &[String],
        limit: usize,
    ) -> Result<Vec<SearchRow>, RepoError>;

    /// List outgoing edges originating from `markdown_uuid`. Each
    /// returned [`EdgeRowOut`] pairs the raw edge with whatever
    /// destination metadata the UI needs to render an "outgoing
    /// destinations" list (today: the destination markdown's title).
    /// Returns an empty Vec when the doc has no outgoing edges, when
    /// the edges table is missing (old data root), or — by default —
    /// when the impl doesn't support edges at all.
    async fn outgoing_edges(&self, _markdown_uuid: &str) -> Result<Vec<EdgeRowOut>, RepoError> {
        Ok(Vec::new())
    }

    /// Append a feedback row. The default impl returns
    /// [`RepoError::ReadOnly`]; only [`crate::dolt_repo::DoltRepo`]
    /// overrides it.
    async fn insert_feedback(&self, _row: FeedbackRow) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// List `sync_jobs` rows. When `only_active` is true, returns only
    /// rows in `pending` or `running` state — used by the UI's polling
    /// chrome. Otherwise returns the most recent `limit` rows newest-first.
    /// Default impl returns [`RepoError::ReadOnly`].
    async fn list_jobs(
        &self,
        _only_active: bool,
        _limit: usize,
    ) -> Result<Vec<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Fetch a single sync job by id. Returns `Ok(None)` when not found.
    async fn get_job(&self, _job_id: &str) -> Result<Option<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Enqueue a new `pending` sync job. Implementations stamp the id
    /// (UUIDv4) and `created_at` themselves so callers don't have to.
    /// The new row is returned as written.
    async fn enqueue_job(
        &self,
        _kind: &str,
        _source_name: Option<&str>,
    ) -> Result<SyncJobRow, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Request cancellation of a pending/running job. Flips `state` to
    /// `canceled` for pending/running rows; the worker observes the
    /// state change on its next poll and SIGTERMs its child.
    async fn request_cancel_job(&self, _job_id: &str) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }
}

/// One outgoing edge, joined with the destination markdown's metadata
/// for direct UI rendering. Producers fill `edge` from the `edges`
/// table; `dst_title` is the destination's `markdowns.title` (or
/// `conversation_name` from the canonical grid_row when title is null),
/// so the UI doesn't have to round-trip a second request per edge.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EdgeRowOut {
    /// The raw edge from the `edges` table.
    #[serde(flatten)]
    pub edge: EdgeRow,
    /// Human-readable title of the destination markdown. `None` when
    /// the destination is missing from `markdowns` (dangling FK — e.g.
    /// the destination was deleted but the edge wasn't pruned).
    pub dst_title: Option<String>,
}

/// Convenience type alias for the dyn-dispatched repo handle used by
/// HTTP handlers via `axum::State`.
pub type DynRepo = Arc<dyn MirrorRepo>;
