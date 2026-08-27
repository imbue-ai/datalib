//! [`IndexRepo`] — the seam to the grid index: `grid_rows`,
//! `markdowns`, `edges`.
//!
//! Reads only, because the `grid_index` step is the file's only writer.
//! The application stores live behind `datalib_core::repo::AppRepo`,
//! which is a different file with a different writer; the two share
//! [`RepoError`] and nothing else.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db::ChatMeta;
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::search::SearchRow;
use datalib_core::repo::RepoError;
use datalib_schema::edges::EdgeRow;

/// Reads of the grid index: `grid_rows`, `markdowns`, `edges`.
///
/// Nothing here writes. The `grid_index` step is the index's only
/// writer, which is what lets any number of readers open the file at
/// once — see `DoltRepo::open` for the constraint that forces it.
#[async_trait]
pub trait IndexRepo: Send + Sync {
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

    /// List rendered documents (the `markdowns` table), newest first,
    /// for the document-picker card. Returns an empty Vec for an empty
    /// or missing store — like [`grid_row_refs`](Self::grid_row_refs),
    /// a bare data root just means there's nothing to pick yet.
    async fn list_docs(&self, _limit: usize) -> Result<Vec<DocRow>, RepoError> {
        Ok(Vec::new())
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

/// One `markdowns` row projected for the document-picker card: just
/// enough to render a pickable list (title, provenance, recency) and
/// address the document (`markdown_uuid`, the same UUID
/// `/api/chat/{markdown_uuid}` takes).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DocRow {
    pub markdown_uuid: String,
    /// Human-readable title; `None` when the renderer didn't set one.
    pub title: Option<String>,
    pub kind: String,
    pub provider: String,
    pub created_at: Option<String>,
}

/// Convenience alias for the dyn-dispatched index handle used by HTTP
/// handlers via `axum::State`.
pub type DynIndexRepo = Arc<dyn IndexRepo>;
